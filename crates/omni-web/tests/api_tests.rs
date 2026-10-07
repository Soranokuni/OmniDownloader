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
    assert_eq!(queued.parent_job_id, Some(id), "the mail view files it under its article (plan P7.6)");

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
// ==========================================
// MCR mail view (plan P7.8)
// ==========================================

/// A Message-ID in a query string.
fn q(key: &str) -> String {
    key.replace('%', "%25").replace('<', "%3C").replace('>', "%3E").replace('@', "%40").replace('+', "%2B")
}

/// A handled mail from GEORGIOU whose first link became a job; returns the job id.
fn seed_mail(app: &App, key: &str, body: &str, job_url: &str) -> Result<i64> {
    let mut new = omni_core::models::NewJob::new(job_url, "1_GEORGIOU_SEISMOS", "GEORGIOU");
    new.keyword = "SEISMOS".into();
    new.email_message_id = Some(key.into());
    let result = app.repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)?;
    app.repo.record_processed_mail(&omni_core::models::ProcessedMail {
        internet_message_id: key.into(),
        source_id: Some("AAMk-graph-id".into()),
        outcome: "JOBS".into(),
        from_address: Some("e.georgiou@example.gr".into()),
        from_name: Some("Ελένη Γεωργίου".into()),
        subject: Some("ΘΕΜΑΤΑ ΕΛΕΝΗΣ".into()),
        jobs_json: json!([{ "index_str": "1", "slug": "1_GEORGIOU_SEISMOS", "url": job_url, "status": "PENDING", "result": result }])
            .to_string(),
        received_at: Some(chrono::Utc::now()),
        body_text: Some(body.into()),
        parse_json: Some(
            json!({ "journalist": "GEORGIOU", "how": "sender", "outcome": "JOBS",
                    "sections": [{ "index_str": "1", "keyword": "SEISMOS" }] })
            .to_string(),
        ),
        ..Default::default()
    })?;
    Ok(result.job_id())
}

#[tokio::test]
async fn the_mail_view_lists_a_mail_and_shows_its_text_links_and_jobs() -> Result<()> {
    let app = App::new()?;
    let body = "1. ΣΕΙΣΜΟΣ\nhttps://www.youtube.com/watch?v=api00000001\nΔείτε και https://www.portal-news.example/";
    let id = seed_mail(&app, "<m1@example.gr>", body, "https://www.youtube.com/watch?v=api00000001")?;

    let (status, list) = app.send("GET", "/api/mails", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["entries"][0]["key"], "<m1@example.gr>");
    assert_eq!(list["entries"][0]["state"], "active");
    assert_eq!(list["entries"][0]["jobs"][0]["id"], id);
    assert_eq!(list["counts"]["all"], 1);
    assert!(list["job_counts"]["active"].as_i64().is_some());

    let uri = format!("/api/mails/view?key={}", q("<m1@example.gr>"));
    let (status, view) = app.send("GET", &uri, &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["from_name"], "Ελένη Γεωργίου");
    assert_eq!(view["how"], "από τη διεύθυνση του αποστολέα");
    assert_eq!(view["links"][0]["jobs"], json!([id]));
    assert_eq!(view["links"][1]["skip"]["can_queue"], true);
    assert_eq!(view["jobs"][0]["id"], id);
    assert_eq!(view["jobs"][0]["place"]["link"], "l1");
    assert_eq!(view["next_index"], "2");
    assert_eq!(view["text"][0]["role"], "read");

    // The live refresh carries what changes, not the text.
    let (_, part) = app.send("GET", &format!("{uri}&parts=jobs"), &app.mcr_token, None).await?;
    assert!(part.get("text").is_none() && part["jobs"][0]["id"] == id, "{part}");

    let (status, _) = app.send("GET", "/api/mails/view?key=%3Cnope%40x%3E", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// The mail's text reaches the panel as JSON strings, never as markup.
#[tokio::test]
async fn a_hostile_mail_text_is_returned_as_data() -> Result<()> {
    let app = App::new()?;
    let body = "<img src=x onerror=alert(1)>\nhttps://www.youtube.com/watch?v=xss00000002\n</div><script>alert(2)</script>";
    seed_mail(&app, "<x1@example.gr>", body, "https://www.youtube.com/watch?v=xss00000002")?;
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()))
        .oneshot(
            Request::builder()
                .uri(format!("/api/mails/view?key={}", q("<x1@example.gr>")))
                .header("cookie", format!("omni_session={}", app.mcr_token))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.headers()["content-type"], "application/json");
    let view: Value = serde_json::from_slice(&res.into_body().collect().await?.to_bytes())?;
    assert_eq!(view["text"][0]["lines"][0][0]["t"], "<img src=x onerror=alert(1)>");
    assert_eq!(view["text"][0]["lines"][2][0]["t"], "</div><script>alert(2)</script>");
    Ok(())
}

#[tokio::test]
async fn a_skipped_link_can_be_queued_from_its_mail_once() -> Result<()> {
    let app = App::new()?;
    let body = "1. ΣΕΙΣΜΟΣ\nhttps://www.youtube.com/watch?v=api00000003\nΔείτε και https://www.portal-news.example/\nΦωτο: https://www.example.gr/photo.jpg";
    seed_mail(&app, "<m3@example.gr>", body, "https://www.youtube.com/watch?v=api00000003")?;
    let send = |url: &str| json!({ "key": "<m3@example.gr>", "url": url });

    // Not in this mail: refused.
    let (status, _) = app
        .send("POST", "/api/mails/queue-link", &app.mcr_token, Some(send("https://evil.example/x")))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A photo: refused.
    let (status, _) = app
        .send("POST", "/api/mails/queue-link", &app.mcr_token, Some(send("https://www.example.gr/photo.jpg")))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = app
        .send("POST", "/api/mails/queue-link", &app.mcr_token, Some(send("https://www.portal-news.example/")))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["slug"], "2_GEORGIOU_SEISMOS", "next number, the mail's journalist, the story next to it");
    let job = app.repo.get_job(body["job_id"].as_i64().unwrap())?.unwrap();
    assert_eq!(job.email_message_id.as_deref(), Some("<m3@example.gr>"));
    assert_eq!(job.status, JobStatus::Pending);

    // Filed under its link in the view; a second click is refused.
    let (_, view) = app
        .send("GET", &format!("/api/mails/view?key={}", q("<m3@example.gr>")), &app.mcr_token, None)
        .await?;
    assert_eq!(view["links"][1]["jobs"], json!([job.id]), "{view}");
    let (status, _) = app
        .send("POST", "/api/mails/queue-link", &app.mcr_token, Some(send("https://www.portal-news.example/")))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn only_a_mail_the_watcher_gave_up_on_can_be_read_again_from_the_desk() -> Result<()> {
    let app = App::new()?;
    seed_mail(&app, "<ok@example.gr>", "x https://youtu.be/okokokok", "https://youtu.be/okokokok")?;
    app.repo.record_processed_mail(&omni_core::models::ProcessedMail {
        internet_message_id: "<failed@example.gr>".into(),
        source_id: Some("AAMk-failed".into()),
        outcome: "FAILED".into(),
        subject: Some("Πλάνα λιμάνι".into()),
        ..Default::default()
    })?;
    let ask = |key: &str| json!({ "key": key });

    let (status, _) = app.send("POST", "/api/mails/reprocess", &app.mcr_token, Some(ask("<ok@example.gr>"))).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a handled mail stays with the administrator");
    let (status, _) = app.send("POST", "/api/mails/reprocess", &app.mcr_token, Some(ask("<nope@example.gr>"))).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) =
        app.send("POST", "/api/mails/reprocess", &app.mcr_token, Some(ask("<failed@example.gr>"))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pending: Vec<String> = app.repo.pending_mail_reprocess()?.into_iter().map(|(k, _, _)| k).collect();
    assert_eq!(pending, vec!["<failed@example.gr>"]);
    Ok(())
}

#[tokio::test]
async fn a_link_added_by_hand_is_an_entry_of_its_own_with_a_timeline() -> Result<()> {
    let app = App::new()?;
    let (status, created) = app
        .send(
            "POST",
            "/api/jobs",
            &app.mcr_token,
            Some(json!({ "url": "https://www.ertnews.gr/video/kairos/", "journalist": "MCR", "keyword": "KAIROS" })),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["job_id"].as_i64().unwrap();

    let (_, list) = app.send("GET", "/api/mails", &app.mcr_token, None).await?;
    assert_eq!(list["entries"][0]["kind"], "manual");
    assert_eq!(list["entries"][0]["key"], id.to_string());
    assert_eq!(list["entries"][0]["added_by_name"], "MCR Desk");

    let (status, view) =
        app.send("GET", &format!("/api/mails/view?kind=manual&key={id}"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["links"][0]["jobs"], json!([id]));
    assert_eq!(view["jobs"][0]["place"]["shared"], false);

    let (status, detail) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(detail["events"][0]["message"].as_str().unwrap_or("").starts_with("Queued as"), "{detail}");
    Ok(())
}
