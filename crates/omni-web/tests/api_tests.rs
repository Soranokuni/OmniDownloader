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
    busy: omni_core::busy::BusyJobs,
    _dir: TempDir,
}

impl App {
    fn new() -> Result<Self> {
        Self::with_repair_mode(omni_core::config::RepairMode::Hold)
    }

    fn with_repair_mode(mode: omni_core::config::RepairMode) -> Result<Self> {
        Self::with_config(|config| config.repair.mode = mode)
    }

    fn with_config(tweak: impl FnOnce(&mut AppConfig)) -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let mut config = AppConfig::default();
        tweak(&mut config);

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

        let state = AppState::new(repo.clone(), config, dir.path().join("config.json"))
            .with_temp_dir(dir.path().join("temp"));
        let busy = state.busy_jobs.clone();
        let router = WebServer::build_router(state);

        Ok(Self {
            router,
            repo,
            mcr_token,
            user_token,
            admin_token,
            busy,
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
    // The desk's add form is the normal start of a job, not a touch (P11.0).
    let made = app.repo.get_job(job_id)?.unwrap();
    assert_eq!((made.human_touched, made.touch_reason), (false, None));

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

    // 4b. A running job is not retried or overridden
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    for (path, body) in [
        ("retry", None),
        ("override", Some(json!({"url": "https://www.youtube.com/watch?v=other"}))),
    ] {
        let (status, json) = app
            .send("POST", &format!("/api/jobs/{}/{}", job_id, path), &app.mcr_token, body)
            .await?;
        assert_eq!(status, StatusCode::CONFLICT, "{path}");
        assert_eq!(json["error"]["code"], "NOT_RETRYABLE", "{path}");
        let row = app.repo.get_job(job_id)?.unwrap();
        assert_eq!(row.status, JobStatus::Running);
        assert_eq!(row.lease_owner.as_deref(), Some("test-worker"));
    }

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
        // The wall board (P12.13), marked by its own script.
        ("/board", &app.mcr_token, "/static/board.js?v="),
        // Its words come from the dictionary since P12.10: the page's own script marks it.
        ("/user", &app.user_token, "/static/user.js?v="),
        // Its words come from the admin dictionaries since P13.6: the page's own script marks it.
        ("/admin", &app.admin_token, "/static/admin.js?v="),
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
    // MCR had to queue it by hand (P11.0), and so acted on the article's own job too
    // (P11.5 review): no second chance may re-run it.
    assert_eq!((queued.human_touched, queued.touch_reason.as_deref()), (true, Some("queued")));
    let parent = app.repo.get_job(id)?.unwrap();
    assert_eq!((parent.human_touched, parent.touch_reason.as_deref()), (true, Some("offer")));

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

/// P11.5 review (F2): MCR queued an offered video from a job waiting in
/// review; a later yt-dlp update must not re-run that job, and the autonomy
/// report counts it as one a person acted on.
#[tokio::test]
async fn a_job_whose_offer_mcr_queued_gets_no_second_chance() -> Result<()> {
    let app = App::new()?;
    let since = chrono::Utc::now() - chrono::Duration::minutes(1);
    let id = app.repo.enqueue(&omni_core::models::NewJob::new("https://www.portal.example/story/2", "1A_MCR_SEISMOS", "MCR"), 24)?.job_id();
    assert!(app.repo.lease_job_by_id(id, "host:1:0", 180)?);
    assert!(app.repo.finish(id, "host:1:0", JobStatus::RequiresReview, Some("NO_STREAM_FOUND"), Some("x"), None)?);
    let raw = "https://cdn.portal.example/video/other.m3u8";
    app.repo.set_candidates(id, Some(&json!([{ "url": raw, "index_str": "1B", "queued_job_id": null }]).to_string()))?;

    let (status, body) = app.send("POST", &format!("/api/jobs/{id}/offers/queue"), &app.mcr_token, Some(json!({ "url": raw }))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(omni_core::second_chance::after_ytdlp_update(&app.repo, "0123456789abcdef"), 0);
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::RequiresReview, "a job MCR settled was re-queued");

    assert!(app.repo.mark_completed_manually(id)?);
    let report = app.repo.autonomy_report(since)?;
    assert_eq!((report.totals.delivered_manual, report.totals.delivered_untouched), (1, 0));
    assert!(report.by_touch_reason.iter().any(|c| c.key == "offer"), "{:?}", report.by_touch_reason);
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
    // MCR queued a link the parser skipped (P11.0): touched. The mail's own job is not.
    assert_eq!((job.human_touched, job.touch_reason.as_deref()), (true, Some("queued")));
    let from_mail = app.repo.get_all_jobs()?.into_iter().find(|j| j.slug == "1_GEORGIOU_SEISMOS").unwrap();
    assert_eq!((from_mail.human_touched, from_mail.touch_reason), (false, None));

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

/// P11.11a: MCR filed a mail from an unknown sender under a journalist and asks the
/// system to remember the address. The next mail from it is filed under them; an
/// address that is someone else's is never moved.
#[tokio::test]
async fn mcr_remembers_an_unknown_sender_for_the_journalist_it_chose() -> Result<()> {
    let app = App::new()?;
    app.repo.save_journalist("GEORGIOU", "Eleni Georgiou", &["e.georgiou@example.gr".into()], 0)?;
    app.repo.save_journalist("NIKOLAOU", "Giorgos Nikolaou", &[], 0)?;
    let mail = |key: &str, from: &str| omni_core::models::ProcessedMail {
        internet_message_id: key.into(),
        source_id: Some("AAMk-x".into()),
        outcome: "JOBS".into(),
        from_address: Some(from.into()),
        subject: Some("Πλάνα".into()),
        ..Default::default()
    };
    app.repo.record_processed_mail(&mail("<new@example.gr>", "Freelance.Crew@Example.net"))?;
    app.repo.record_processed_mail(&mail("<known@example.gr>", "e.georgiou@example.gr"))?;
    let view = |key: &str| format!("/api/mails/view?key={}", q(key));
    let ask = |key: &str, surname: &str| json!({ "key": key, "surname": surname });

    let (_, v) = app.send("GET", &view("<new@example.gr>"), &app.mcr_token, None).await?;
    assert_eq!(v["sender_on_roster"], false, "the desk offers the button: {v}");

    // MCR is where unknown mail goes, never a journalist to remember.
    let (status, _) = app.send("POST", "/api/mails/remember-sender", &app.mcr_token, Some(ask("<new@example.gr>", "MCR"))).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = app.send("POST", "/api/mails/remember-sender", &app.mcr_token, Some(ask("<nope@example.gr>", "NIKOLAOU"))).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) =
        app.send("POST", "/api/mails/remember-sender", &app.mcr_token, Some(ask("<new@example.gr>", "nikolaou"))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!((body["surname"].as_str(), body["already"].as_bool()), (Some("NIKOLAOU"), Some(false)));
    assert_eq!(app.repo.find_journalist_by_email("freelance.crew@example.net")?.as_deref(), Some("NIKOLAOU"));
    let (_, v) = app.send("GET", &view("<new@example.gr>"), &app.mcr_token, None).await?;
    assert_eq!(v["sender_on_roster"], true);
    // A second click changes nothing.
    let (status, body) =
        app.send("POST", "/api/mails/remember-sender", &app.mcr_token, Some(ask("<new@example.gr>", "NIKOLAOU"))).await?;
    assert_eq!((status, body["already"].as_bool()), (StatusCode::OK, Some(true)));

    // GEORGIOU's address is not moved to NIKOLAOU.
    let (status, body) =
        app.send("POST", "/api/mails/remember-sender", &app.mcr_token, Some(ask("<known@example.gr>", "NIKOLAOU"))).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(app.repo.find_journalist_by_email("e.georgiou@example.gr")?.as_deref(), Some("GEORGIOU"));
    let nikolaou = app.repo.list_journalists()?.into_iter().find(|j| j.surname == "NIKOLAOU").unwrap();
    assert_eq!(nikolaou.emails, vec!["freelance.crew@example.net"]);
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

/// Plan P7.11: a video on the desk says which mail it came from, and the
/// mail list can be asked for the page that holds that mail.
#[tokio::test]
async fn a_video_on_the_desk_names_its_mail_and_the_list_finds_it() -> Result<()> {
    let app = App::new()?;
    let id = seed_mail(&app, "<m9@example.gr>", "1. Α\nhttps://youtu.be/m9m9m9m9", "https://youtu.be/m9m9m9m9")?;
    let (status, page) = app.send("GET", "/api/jobs?view=live", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK);
    let job = page["jobs"].as_array().unwrap().iter().find(|j| j["id"] == id).unwrap();
    assert_eq!(job["mail_subject"], "ΘΕΜΑΤΑ ΕΛΕΝΗΣ");
    assert_eq!(job["email_message_id"], "<m9@example.gr>");

    let (status, list) = app
        .send("GET", &format!("/api/mails?per_page=5&focus=mail%3A{}", q("<m9@example.gr>")), &app.mcr_token, None)
        .await?;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["page"], 1);
    assert_eq!(list["entries"][0]["key"], "<m9@example.gr>");
    Ok(())
}

/// Plan P7.19: a waiting video says its place; one in back-off has none.
#[tokio::test]
async fn waiting_videos_get_a_queue_position_and_back_off_ones_do_not() -> Result<()> {
    let app = App::new()?;
    let new = |n: u32| omni_core::models::NewJob::new(format!("https://example.gr/q{n}"), format!("{n}_ANNA_QUEUE"), "ANNA");
    let backoff = app.repo.enqueue(&new(1), 24)?.job_id();
    app.repo.lease_job("W", 180)?.expect("leasable");
    assert!(app.repo.requeue_after(backoff, "W", chrono::Duration::minutes(5), "E_TEST", "later")?);
    let first = app.repo.enqueue(&new(2), 24)?.job_id();
    let second = app.repo.enqueue(&new(3), 24)?.job_id();

    let (status, page) = app.send("GET", "/api/jobs?view=live", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK);
    let find = |id: i64| page["jobs"].as_array().unwrap().iter().find(|j| j["id"] == id).unwrap().clone();
    assert_eq!(find(first)["queue_position"], 1);
    assert_eq!(find(second)["queue_position"], 2);
    assert!(find(backoff).get("queue_position").is_none(), "{}", find(backoff));
    Ok(())
}

#[tokio::test]
async fn the_mail_view_gives_a_queue_position_to_ready_videos_only() -> Result<()> {
    let app = App::new()?;
    let ready = seed_mail(&app, "<m19@example.gr>", "1. Α\nhttps://example.gr/q19a", "https://example.gr/q19a")?;
    let mut other = omni_core::models::NewJob::new("https://example.gr/q19b", "2_GEORGIOU_SEISMOS", "GEORGIOU");
    other.email_message_id = Some("<m19@example.gr>".into());
    let waiting = app.repo.enqueue(&other, 24)?.job_id();
    // Lease the second one by hand so it can go into back-off.
    app.repo.lease_job("W", 180)?; // takes `ready`
    app.repo.lease_job("W2", 180)?; // takes `waiting`
    assert!(app.repo.requeue_after(waiting, "W2", chrono::Duration::minutes(5), "E_TEST", "later")?);
    assert!(app.repo.requeue_after(ready, "W", chrono::Duration::seconds(-1), "E_TEST", "later")?);

    let uri = format!("/api/mails/view?key={}", q("<m19@example.gr>"));
    let (status, view) = app.send("GET", &uri, &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    let find = |id: i64| view["jobs"].as_array().unwrap().iter().find(|j| j["id"] == id).unwrap().clone();
    assert_eq!(find(ready)["queue_position"], 1, "{view}");
    assert!(find(waiting).get("queue_position").is_none(), "{view}");
    Ok(())
}

#[tokio::test]
async fn mcr_can_mark_a_review_video_as_put_into_dalet_once() -> Result<()> {
    let app = App::new()?;
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://example.gr/locker", "1_NIKOLAOU_LOCKER", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");

    // A running job is refused.
    let uri = format!("/api/jobs/{id}/mark-done");
    let (status, json) = app.send("POST", &uri, &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "NOT_MARKABLE");
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::Running);

    app.repo
        .finish(id, "test-worker", JobStatus::RequiresReview, Some("E_TEST"), Some("why"), None)?;
    let (status, _) = app.send("POST", &uri, &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::CompletedManual);
    let events = app.repo.get_job_events(id, 100)?;
    assert!(
        events.iter().any(|e| e.message.starts_with("Finished as COMPLETED_MANUAL: put into Dalet by hand")),
        "no event recorded"
    );

    let (status, json) = app.send("POST", &uri, &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "NOT_MARKABLE");
    Ok(())
}

#[tokio::test]
async fn mcr_cancels_a_waiting_video_and_it_stays_in_the_list() -> Result<()> {
    let app = App::new()?;
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://example.gr/cancel", "1_NIKOLAOU_CANCEL", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    let uri = format!("/api/jobs/{id}/cancel");
    let (status, json) = app.send("POST", &uri, &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::Cancelled);
    let events = app.repo.get_job_events(id, 100)?;
    assert!(events.iter().any(|e| e.level == "WARN" && e.message.starts_with("Cancelled by")), "no event");

    let (status, json) = app.send("POST", &uri, &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "NOT_CANCELLABLE");
    Ok(())
}

#[tokio::test]
async fn a_video_that_is_delivering_is_too_late_to_cancel() -> Result<()> {
    let app = App::new()?;
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://example.gr/late", "1_NIKOLAOU_LATE", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    assert!(app.repo.set_stage(id, "test-worker", omni_core::models::JobStage::Deliver)?);

    let (status, json) = app.send("POST", &format!("/api/jobs/{id}/cancel"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "TOO_LATE");
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::Running);
    Ok(())
}

#[tokio::test]
async fn cancel_and_discard_fire_the_running_workers_token() -> Result<()> {
    let app = App::new()?;
    for (n, discard) in [(1, false), (2, true)] {
        let id = app
            .repo
            .enqueue(
                &omni_core::models::NewJob::new(&format!("https://example.gr/kill{n}"), &format!("{n}_NIKOLAOU_KILL"), "NIKOLAOU"),
                omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
            )?
            .job_id();
        app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
        let guard = app.busy.guard(id, "test-worker");
        let token = guard.token();
        assert!(!token.is_cancelled());

        let (status, json) = if discard {
            app.send("POST", &format!("/api/jobs/{id}/discard"), &app.mcr_token, None).await?
        } else {
            app.send("POST", &format!("/api/jobs/{id}/cancel"), &app.mcr_token, None).await?
        };
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(token.is_cancelled(), "the worker's tool would run on (discard={discard})");
    }
    Ok(())
}

#[tokio::test]
async fn a_job_whose_worker_is_still_stopping_is_not_queued_again() -> Result<()> {
    let app = App::new()?;
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://example.gr/stop", "1_NIKOLAOU_STOP", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    assert!(app.repo.cancel_job(id)? == omni_core::repository::CancelOutcome::Cancelled);
    let busy_guard = app.busy.guard(id, "test-worker");

    for (uri, body) in [
        (format!("/api/jobs/{id}/retry"), None),
        (format!("/api/jobs/{id}/override"), Some(serde_json::json!({ "url": "https://example.gr/other" }))),
        (format!("/api/jobs/{id}/redownload"), None),
    ] {
        let (status, json) = app.send("POST", &uri, &app.mcr_token, body).await?;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {json}");
        assert_eq!(json["error"]["code"], "STILL_STOPPING", "{uri}");
    }
    let row = app.repo.get_job(id)?.unwrap();
    assert_eq!(row.status, JobStatus::Cancelled, "the row must not be touched");

    drop(busy_guard);
    let (status, json) = app.send("POST", &format!("/api/jobs/{id}/retry"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::Pending);
    Ok(())
}

#[tokio::test]
async fn mcr_can_hide_old_review_videos_and_show_them_again() -> Result<()> {
    let app = App::new()?;
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://example.gr/old", "1_NIKOLAOU_OLD", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    app.repo.finish(id, "test-worker", JobStatus::RequiresReview, Some("E_TEST"), Some("why"), None)?;

    // The default (72 h) leaves a fresh job alone.
    let (status, json) = app.send("POST", "/api/jobs/review/hide-old", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["hidden"], 0);

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let (status, json) = app
        .send("POST", "/api/jobs/review/hide-old", &app.mcr_token, Some(json!({ "hours": 0 })))
        .await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["hidden"], 1);

    let (_, page) = app.send("GET", "/api/jobs?view=review", &app.mcr_token, None).await?;
    assert_eq!(page["total"], 0, "{page}");
    assert_eq!(page["counts"]["review"], 0);
    assert_eq!(page["counts"]["review_hidden"], 1);
    assert!(page["counts"]["completed_today"].as_i64().is_some(), "{page}");

    let (_, page) = app.send("GET", "/api/jobs?view=review&hidden=1", &app.mcr_token, None).await?;
    assert_eq!(page["total"], 1, "{page}");
    assert!(page["jobs"][0]["cleared_at"].is_string(), "the card can say it is hidden: {page}");
    Ok(())
}

#[tokio::test]
async fn the_self_check_panel_lists_thirty_days_of_self_heal() -> Result<()> {
    let app = App::new()?;
    let (_, json) = app.send("GET", "/api/admin/selfcheck", &app.admin_token, None).await?;
    assert_eq!(json["selfheal"], json!([]));

    let now = chrono::Utc::now();
    app.repo.record_selfheal("old.example", "nothing", None, None, 0, None, now - chrono::Duration::days(31))?;
    app.repo.record_selfheal("news.example", "found", Some("0123abcd"), Some("llm"), 3, None, now - chrono::Duration::days(3))?;
    let (_, json) = app.send("GET", "/api/admin/selfcheck", &app.admin_token, None).await?;
    let rows = json["selfheal"].as_array().expect("selfheal array");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0]["domain"].as_str(), rows[0]["outcome"].as_str(), rows[0]["origin"].as_str()), (Some("news.example"), Some("found"), Some("llm")));
    assert_eq!(rows[0]["pages"], 3);
    Ok(())
}

#[tokio::test]
async fn the_self_check_panel_lists_the_site_canaries() -> Result<()> {
    let app = App::new()?;
    let (status, json) = app.send("GET", "/api/admin/selfcheck", &app.admin_token, None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["canaries"], json!([]));

    app.repo.add_canary("news.example", "https://news.example/a", None, chrono::Utc::now())?;
    let id = app.repo.list_canaries()?[0].id;
    app.repo.record_canary_result(id, false, "no video found on the page")?;
    let (_, json) = app.send("GET", "/api/admin/selfcheck", &app.admin_token, None).await?;
    let rows = json["canaries"].as_array().expect("canaries array");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["domain"], "news.example");
    assert_eq!(rows[0]["url"], "https://news.example/a");
    assert_eq!(rows[0]["last_ok"], false);
    assert_eq!(rows[0]["last_detail"], "no video found on the page");
    assert!(rows[0]["last_checked_at"].is_string());
    // The existing fields are still there.
    assert!(json["links"].is_array());
    Ok(())
}

// ---- videos held for approval (P10.5) ----

const HELD_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A job held for approval, with a file in its held directory.
fn held_job(app: &App, n: u32) -> Result<i64> {
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new(&format!("https://example.gr/h{n}"), &format!("{n}_NIKOLAOU_HELD"), "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    let dir = omni_core::held::held_dir(&app._dir.path().join("temp"), id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("clip.mxf"), b"abc")?;
    assert!(app.repo.hold_output(
        id,
        "test-worker",
        &dir.join("clip.mxf").to_string_lossy(),
        3,
        HELD_SHA,
        Some(2.0),
        chrono::Utc::now() + chrono::Duration::hours(24),
    )?);
    assert!(app.repo.finish(id, "test-worker", JobStatus::AwaitingApproval, None, None, None)?);
    Ok(id)
}

#[tokio::test]
async fn the_approval_view_lists_only_held_jobs_with_their_held_block() -> Result<()> {
    let app = App::new()?;
    let held = held_job(&app, 1)?;
    // A job in review must not show up in the approval view.
    let other = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://example.gr/r1", "2_NIKOLAOU_REVIEW", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    app.repo.finish(other, "test-worker", JobStatus::RequiresReview, Some("E_TEST"), Some("why"), None)?;

    let (status, page) = app.send("GET", "/api/jobs?view=approval", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], 1, "{page}");
    assert_eq!(page["jobs"][0]["id"], held, "{page}");
    assert_eq!(page["jobs"][0]["held"]["sha256"], HELD_SHA, "{page}");
    assert_eq!(page["counts"]["approval"], 1, "{page}");
    assert_eq!(page["counts"]["review"], 1, "{page}");

    // The review list does not contain the held job.
    let (_, review) = app.send("GET", "/api/jobs?view=review", &app.mcr_token, None).await?;
    assert_eq!(review["total"], 1, "{review}");
    assert_eq!(review["jobs"][0]["id"], other, "{review}");
    Ok(())
}

#[tokio::test]
async fn the_desk_sees_the_held_file_and_approves_exactly_that_sha() -> Result<()> {
    let app = App::new()?;
    let id = held_job(&app, 1)?;

    let (status, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    let held = &json["job"]["held"];
    assert_eq!(held["sha256"], HELD_SHA, "{json}");
    assert_eq!(held["bytes"], 3);
    assert!(held["expires_at"].is_string() && held["created_at"].is_string(), "{held}");

    // The wrong file, and a malformed hash, change nothing.
    let (status, json) = app
        .send("POST", &format!("/api/jobs/{id}/approve"), &app.mcr_token, Some(json!({ "sha256": "0".repeat(64) })))
        .await?;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "NOT_HELD");
    let (status, _) = app
        .send("POST", &format!("/api/jobs/{id}/approve"), &app.mcr_token, Some(json!({ "sha256": "xyz" })))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval);

    let (status, json) = app
        .send("POST", &format!("/api/jobs/{id}/approve"), &app.mcr_token, Some(json!({ "sha256": HELD_SHA.to_uppercase() })))
        .await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::Pending);
    assert!(app.repo.approved_held(id)?.is_some());

    // Once decided, the job JSON carries no held block and a second approval is a 409.
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert!(json["job"]["held"].is_null(), "{json}");
    let (status, json) = app
        .send("POST", &format!("/api/jobs/{id}/approve"), &app.mcr_token, Some(json!({ "sha256": HELD_SHA })))
        .await?;
    assert_eq!((status, json["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("NOT_HELD")), "{json}");
    Ok(())
}

#[tokio::test]
async fn rejecting_sends_the_job_to_review_and_deletes_the_held_file() -> Result<()> {
    let app = App::new()?;
    let id = held_job(&app, 1)?;
    let held_dir = omni_core::held::held_dir(&app._dir.path().join("temp"), id);
    assert!(held_dir.exists());

    // An unknown reason and an over-long note are refused before anything changes.
    let (status, _) = app
        .send("POST", &format!("/api/jobs/{id}/reject"), &app.mcr_token, Some(json!({ "reason": "because" })))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/jobs/{id}/reject"),
            &app.mcr_token,
            Some(json!({ "reason": "other", "note": "x".repeat(301) })),
        )
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval);
    assert!(held_dir.exists());

    let (status, json) = app
        .send(
            "POST",
            &format!("/api/jobs/{id}/reject"),
            &app.mcr_token,
            Some(json!({ "reason": "wrong_video", "note": "άλλο θέμα" })),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    let job = app.repo.get_job(id)?.unwrap();
    assert_eq!(job.status, JobStatus::RequiresReview);
    assert_eq!(job.error_code.as_deref(), Some("REJECTED_BY_MCR"));
    assert!(job.error_message.as_deref().unwrap().contains("άλλο θέμα"));
    assert!(!held_dir.exists(), "the held directory must be removed");
    assert_eq!(app.repo.get_held(id)?.unwrap().reason.as_deref(), Some("wrong_video"));

    // A job that is not held: 409 NOT_HELD, nothing touched.
    let (status, json) = app
        .send("POST", &format!("/api/jobs/{id}/reject"), &app.mcr_token, Some(json!({ "reason": "other" })))
        .await?;
    assert_eq!((status, json["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("NOT_HELD")), "{json}");
    Ok(())
}

// ---- preview of a held video (P10.6) ----

impl App {
    /// A GET with optional extra headers; the status, headers and raw body.
    async fn get_raw(&self, uri: &str, token: &str, headers: &[(&str, &str)]) -> Result<(StatusCode, axum::http::HeaderMap, Vec<u8>)> {
        let mut builder = Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", format!("omni_session={token}"))
            .header("host", "mcr.local");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let res = self
            .router
            .clone()
            .layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()))
            .oneshot(builder.body(Body::empty())?)
            .await?;
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.into_body().collect().await?.to_bytes().to_vec();
        Ok((status, headers, bytes))
    }
}

/// A held job whose directory also holds a 1000-byte "preview.mp4" and a contact sheet.
fn held_job_with_preview(app: &App) -> Result<(i64, std::path::PathBuf)> {
    let id = held_job(app, 1)?;
    let dir = omni_core::held::held_dir(&app._dir.path().join("temp"), id);
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("_preview.mp4"), &data)?;
    std::fs::write(dir.join("_contact.jpg"), b"jpegbytes")?;
    Ok((id, dir))
}

#[tokio::test]
async fn the_desk_seeks_in_the_preview_with_a_range_request() -> Result<()> {
    let app = App::new()?;
    let (id, _dir) = held_job_with_preview(&app)?;

    let (status, headers, body) = app.get_raw(&format!("/api/jobs/{id}/preview/video"), &app.mcr_token, &[("range", "bytes=0-99")]).await?;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 0-99/1000");
    assert_eq!(body.len(), 100);
    assert_eq!(headers["content-type"], "video/mp4");
    assert_eq!(headers["cache-control"], "no-store");

    let (status, headers, body) = app.get_raw(&format!("/api/jobs/{id}/preview/video"), &app.mcr_token, &[("range", "bytes=900-")]).await?;
    assert_eq!((status, body.len()), (StatusCode::PARTIAL_CONTENT, 100));
    assert_eq!(headers["content-range"], "bytes 900-999/1000");

    let (status, _, body) = app.get_raw(&format!("/api/jobs/{id}/preview/video"), &app.mcr_token, &[]).await?;
    assert_eq!((status, body.len()), (StatusCode::OK, 1000));

    let (status, headers, body) = app.get_raw(&format!("/api/jobs/{id}/preview/contact"), &app.mcr_token, &[]).await?;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, b"jpegbytes".as_slice()));
    assert_eq!(headers["content-type"], "image/jpeg");

    // No article picture yet: 404, not an empty 200.
    let (status, _, _) = app.get_raw(&format!("/api/jobs/{id}/preview/article"), &app.mcr_token, &[]).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The job JSON says what is there.
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(
        json["job"]["held"]["preview"],
        json!({ "video": true, "audio_only": false, "contact": true, "article": false, "frame": false }),
        "{json}"
    );
    Ok(())
}

#[tokio::test]
async fn an_audio_only_preview_is_served_as_audio() -> Result<()> {
    let app = App::new()?;
    let (id, dir) = held_job_with_preview(&app)?;
    std::fs::remove_file(dir.join("_preview.mp4"))?;
    std::fs::write(dir.join("_preview.m4a"), b"audio")?;
    let (status, headers, _) = app.get_raw(&format!("/api/jobs/{id}/preview/video"), &app.mcr_token, &[]).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "audio/mp4");
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(json["job"]["held"]["preview"]["audio_only"], true, "{json}");
    assert_eq!(json["job"]["held"]["preview"]["video"], false, "{json}");
    Ok(())
}

#[tokio::test]
async fn there_is_no_preview_without_a_live_held_row_and_the_request_picks_no_file() -> Result<()> {
    let app = App::new()?;
    // A job with no held row at all, though a file sits where one would be.
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://example.gr/p9", "9_NIKOLAOU_NOROW", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    let dir = omni_core::held::held_dir(&app._dir.path().join("temp"), id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("_preview.mp4"), b"x")?;
    let (status, _, _) = app.get_raw(&format!("/api/jobs/{id}/preview/video"), &app.mcr_token, &[]).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let app = App::new()?;
    // A delivered video no longer has a preview, even if its files linger.
    let (held_id, held_dir) = held_job_with_preview(&app)?;
    let sha = app.repo.waiting_held(held_id)?.unwrap().sha256;
    assert!(app.repo.approve_held(held_id, &sha, "desk")?);
    let (status, _, _) = app.get_raw(&format!("/api/jobs/{held_id}/preview/video"), &app.mcr_token, &[]).await?;
    assert_eq!(status, StatusCode::OK, "an approved video still waits for delivery");
    let job = app.repo.lease_job("w2", 180)?.expect("approved job is leased");
    assert_eq!(job.id, held_id);
    assert!(app.repo.record_held_delivery(held_id, "w2", "C:/watch/x.mxf", 2.0)?);
    assert!(app.repo.finish_held_delivery(held_id, "w2")?);
    assert_eq!(app.repo.get_held(held_id)?.unwrap().decision.as_deref(), Some("delivered"));
    assert!(held_dir.join("_preview.mp4").is_file());
    let (status, _, _) = app.get_raw(&format!("/api/jobs/{held_id}/preview/video"), &app.mcr_token, &[]).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Only the three fixed names are routes: nothing in the request names a file.
    {
        let app2 = App::new()?;
        let r = held_job_with_preview(&app2)?;
        for uri in [
            format!("/api/jobs/{}/preview/..%2F{}.mxf", r.0, "1_NIKOLAOU_HELD"),
            format!("/api/jobs/{}/preview/_preview.mp4", r.0),
            format!("/api/jobs/{}/preview/../preview/video/x", r.0),
            format!("/api/jobs/{}/preview/clip.mxf", r.0),
        ] {
            let (status, _, body) = app2.get_raw(&uri, &app2.mcr_token, &[]).await?;
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
                "{uri} -> {status}"
            );
            assert!(body != b"abc", "{uri} served the held MXF");
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_held_row_outside_the_jobs_held_directory_serves_no_preview() -> Result<()> {
    let app = App::new()?;
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://example.gr/e1", "1_NIKOLAOU_ELSEWHERE", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    let elsewhere = app._dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere)?;
    std::fs::write(elsewhere.join("clip.mxf"), b"abc")?;
    std::fs::write(elsewhere.join("_preview.mp4"), b"secret")?;
    assert!(app.repo.hold_output(
        id,
        "test-worker",
        &elsewhere.join("clip.mxf").to_string_lossy(),
        3,
        HELD_SHA,
        Some(2.0),
        chrono::Utc::now() + chrono::Duration::hours(24),
    )?);
    let (status, _, body) = app.get_raw(&format!("/api/jobs/{id}/preview/video"), &app.mcr_token, &[]).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(body, b"secret");
    Ok(())
}

// ---- the approval card's evidence (P10.11b) ----

#[tokio::test]
async fn the_closest_frame_is_served_to_mcr_only_when_it_exists() -> Result<()> {
    let app = App::new()?;
    let (id, dir) = held_job_with_preview(&app)?;
    let uri = format!("/api/jobs/{id}/preview/frame");

    let (status, _, _) = app.get_raw(&uri, &app.mcr_token, &[]).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "no frame yet: 404, not an empty 200");
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(json["job"]["held"]["preview"]["frame"], false, "{json}");

    std::fs::write(dir.join("_frame.jpg"), b"framebytes")?;
    let (status, headers, body) = app.get_raw(&uri, &app.mcr_token, &[]).await?;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, b"framebytes".as_slice()));
    assert_eq!(headers["content-type"], "image/jpeg");
    assert_eq!(headers["cache-control"], "no-store");
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(json["job"]["held"]["preview"]["frame"], true, "{json}");

    // Who may ask is the MATRIX row of the route.
    Ok(())
}

/// A held job whose repair attempt is on record: found by an admin recipe that
/// MCR approved twice, the held candidate the second one, a picture compared.
fn held_repaired_job(app: &App) -> Result<i64> {
    use omni_core::recipe_score::{Event, NewRecipe, Origin, ScopeKind};
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://news.example.gr/story/1", "1_NIKOLAOU_REPAIR", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    let now = chrono::Utc::now();
    let recipe = NewRecipe {
        scope_kind: ScopeKind::Site,
        scope_key: "example.gr".into(),
        family: None,
        recipe_json: "{}".into(),
        hash: "h-admin-1".into(),
        steps_hash: "s-admin-1".into(),
        dsl_version: 1,
        origin: Origin::Admin,
    };
    let rid = match app.repo.insert_recipe(&recipe, now)? {
        omni_core::recipe_score::InsertOutcome::Inserted(id) => id,
        other => panic!("{other:?}"),
    };
    app.repo.recipe_event(rid, "example.gr", Event::Approval, now)?;
    app.repo.recipe_event(rid, "example.gr", Event::Approval, now)?;
    // Approvals on another domain must not count.
    app.repo.recipe_event(rid, "other.example", Event::Approval, now)?;

    let attempt = app.repo.start_attempt(id, "hold", now)?;
    let list = json!([
        {"url": "https://cdn.news.example.gr/first.mp4", "duration_secs": 30.0, "checks": [{"name": "probe", "ok": false, "detail": "x"}]},
        {"url": "https://cdn.news.example.gr/second.mp4", "duration_secs": 118.0, "checks": [
            {"name": "egress", "ok": true, "detail": "Δημόσιος κόμβος"},
            {"name": "length", "ok": true, "detail": "Ταιριάζει με τα 120 δλ της σελίδας"}
        ]}
    ]);
    let end = omni_core::repository::AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        recipe_id: Some(rid),
        recipe_hash: Some("h-admin-1".into()),
        domain: Some("example.gr".into()),
        candidates_json: Some(list.to_string()),
        held_url: Some("https://cdn.news.example.gr/second.mp4?sig=abc".into()),
        next_index: 1,
        expected_secs: Some(120.0),
        ..Default::default()
    };
    app.repo.finish_attempt(attempt, &end, now)?;
    assert!(app.repo.record_card_evidence(attempt, id, "test-worker", Some(4), Some(7.0), Some("img.news.example.gr"))?);

    let dir = omni_core::held::held_dir(&app._dir.path().join("temp"), id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("clip.mxf"), b"abc")?;
    assert!(app.repo.hold_output(id, "test-worker", &dir.join("clip.mxf").to_string_lossy(), 3, HELD_SHA, Some(118.0), now + chrono::Duration::hours(24))?);
    assert!(app.repo.finish(id, "test-worker", JobStatus::AwaitingApproval, None, None, None)?);
    Ok(id)
}

#[tokio::test]
async fn the_approval_view_carries_the_repair_card_of_the_held_candidate() -> Result<()> {
    let app = App::new()?;
    let id = held_repaired_job(&app)?;
    let (status, page) = app.send("GET", "/api/jobs?view=approval", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{page}");
    let r = &page["jobs"][0]["repair"];
    assert_eq!(page["jobs"][0]["id"], id);
    assert_eq!(r["found"], "recipe", "{page}");
    assert_eq!(r["recipe_approvals"], 2, "only this domain's approvals count: {page}");
    assert_eq!(r["source_host"], "cdn.news.example.gr");
    assert_eq!((r["duration_secs"].as_f64(), r["expected_secs"].as_f64()), (Some(118.0), Some(120.0)));
    let names: Vec<&str> = r["checks"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["egress", "length"], "the held candidate's checks, not the first one's");
    assert_eq!(r["short_clip"], false);
    assert_eq!(r["image"], json!({ "distance": 4, "frame_secs": 7.0, "match": true }), "{page}");
    assert!(!r.to_string().contains("sig=abc") && !r.to_string().contains("second.mp4"), "hosts only: {r}");
    // The single-job view the mail panel uses says the same.
    let (_, one) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(one["job"]["repair"], *r);
    Ok(())
}

#[tokio::test]
async fn a_held_job_without_a_repair_attempt_has_a_null_repair() -> Result<()> {
    let app = App::new()?;
    held_job(&app, 1)?;
    let (_, page) = app.send("GET", "/api/jobs?view=approval", &app.mcr_token, None).await?;
    assert_eq!(page["total"], 1, "{page}");
    assert!(page["jobs"][0]["repair"].is_null(), "{page}");
    assert!(page["jobs"][0]["held"].is_object());
    Ok(())
}

// ---- «Επόμενη επιλογή» (P10.13) ----

/// A held job (see `held_job`) whose repair attempt verified `urls` and held `urls[index]`.
fn held_with_candidates(app: &App, n: u32, urls: &[&str], index: usize) -> Result<i64> {
    let id = held_job(app, n)?;
    let now = chrono::Utc::now();
    let attempt = app.repo.start_attempt(id, "hold", now)?;
    let list: Vec<serde_json::Value> =
        urls.iter().map(|u| json!({"url": u, "origin": "recipe:abcd1234", "duration_secs": 60.0, "checks": []})).collect();
    let end = omni_core::repository::AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        domain: Some("example.gr".into()),
        candidates_json: Some(serde_json::Value::Array(list).to_string()),
        held_url: Some(urls[index].to_string()),
        next_index: index as i64,
        ..Default::default()
    };
    app.repo.finish_attempt(attempt, &end, now)?;
    Ok(id)
}

const NEXT_URLS: [&str; 3] = [
    "https://cdn.news.example.gr/a.mp4",
    "https://cdn.news.example.gr/b.mp4",
    "https://cdn.news.example.gr/c.mp4",
];

#[tokio::test]
async fn the_repair_card_says_how_many_next_choices_are_left() -> Result<()> {
    let app = App::new()?;
    held_with_candidates(&app, 1, &NEXT_URLS, 0)?;
    let (_, page) = app.send("GET", "/api/jobs?view=approval", &app.mcr_token, None).await?;
    assert_eq!(page["jobs"][0]["repair"]["next_left"], 2, "{page}");
    let last = held_with_candidates(&app, 2, &NEXT_URLS, 2)?;
    let (_, one) = app.send("GET", &format!("/api/jobs/{last}"), &app.mcr_token, None).await?;
    assert_eq!(one["job"]["repair"]["next_left"], 0, "the last candidate has nothing after it: {one}");
    Ok(())
}

#[tokio::test]
async fn the_next_choice_needs_the_held_file_and_a_verified_candidate_after_it() -> Result<()> {
    let app = App::new()?;
    let id = held_with_candidates(&app, 1, &NEXT_URLS, 0)?;
    let last = held_with_candidates(&app, 2, &NEXT_URLS, 2)?;
    let held_dir = omni_core::held::held_dir(&app._dir.path().join("temp"), id);
    let post = |sha: String| {
        let uri = format!("/api/jobs/{id}/next");
        let token = app.mcr_token.clone();
        let app = &app;
        async move { app.send("POST", &uri, &token, Some(json!({ "sha256": sha }))).await }
    };

    // A malformed or other file's SHA: nothing changes.
    let (status, _) = post("xyz".into()).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, json) = post("0".repeat(64)).await?;
    assert_eq!((status, json["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("NO_NEXT")), "{json}");
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval);
    assert!(held_dir.exists());

    let (status, json) = post(HELD_SHA.to_uppercase()).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["index"], 1);
    assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::Pending);
    assert!(!held_dir.exists(), "the rejected file goes now");
    let attempt = app.repo.latest_attempt(id)?.unwrap();
    assert_eq!((attempt.rung.as_deref(), attempt.outcome.as_deref(), attempt.next_index), (Some("next"), None, 1));
    // The job is not held any more: a second press is a 409.
    let (status, _) = post(HELD_SHA.to_string()).await?;
    assert_eq!(status, StatusCode::CONFLICT);

    // Nothing left after the last candidate: 409, the video stays held.
    let (status, json) = app
        .send("POST", &format!("/api/jobs/{last}/next"), &app.mcr_token, Some(json!({ "sha256": HELD_SHA })))
        .await?;
    assert_eq!((status, json["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("NO_NEXT")), "{json}");
    assert_eq!(app.repo.get_job(last)?.unwrap().status, JobStatus::AwaitingApproval);
    assert!(app.repo.waiting_held(last)?.is_some());
    Ok(())
}

#[tokio::test]
async fn the_next_choice_is_only_for_hold_mode() -> Result<()> {
    for mode in [omni_core::config::RepairMode::Off, omni_core::config::RepairMode::Shadow] {
        let app = App::with_repair_mode(mode)?;
        let id = held_with_candidates(&app, 1, &NEXT_URLS, 0)?;
        let (_, page) = app.send("GET", "/api/jobs?view=approval", &app.mcr_token, None).await?;
        assert_eq!(page["jobs"][0]["repair"]["next_left"], 0, "{mode:?}: the button is hidden: {page}");
        let (status, json) = app
            .send("POST", &format!("/api/jobs/{id}/next"), &app.mcr_token, Some(json!({ "sha256": HELD_SHA })))
            .await?;
        assert_eq!((status, json["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("REPAIR_OFF")), "{mode:?}: {json}");
        assert!(json["error"]["message"].as_str().unwrap().contains("απενεργοποιημένη"));
        assert_eq!(app.repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval, "{mode:?}: nothing changed");
        assert!(app.repo.waiting_held(id)?.is_some());
    }
    Ok(())
}

#[tokio::test]
async fn the_mcr_form_keeps_the_journalist_identity_and_the_slug_matches_the_stored_fields() -> Result<()> {
    let app = App::new()?;
    let (status, json) = app
        .send(
            "POST",
            "/api/jobs",
            &app.mcr_token,
            Some(json!({
                "url": "https://example.test/slug/1",
                "keyword": "fire-in-the-hills",
                "journalist": "Papa-Daki",
                "index_str": "2-B"
            })),
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    let job = app.repo.get_job(json["job_id"].as_i64().unwrap())?.unwrap();
    // The identity is what the roster knows, as at base.
    assert_eq!(job.journalist, "PAPA-DAKI");
    // The stored fields keep the form's reading; the file name follows the slug rule (P12.11).
    assert_eq!((job.index_str.as_str(), job.keyword.as_str()), ("2-B", "FIRE-IN-THE-HILLS"));
    // The invariant the article code (`house_slug`) relies on.
    assert_eq!(job.slug, omni_core::slug::compose(&job.index_str, &job.journalist, &job.keyword));
    assert_eq!(job.slug, "2_B_PAPA_DAKI_FIRE_IN_THE_HILLS");
    Ok(())
}

/// P12.11b: a typed story number is at most `MAX_TYPED_INDEX` characters, so the A/B/C of a
/// story's further videos always survive in the file name. Longer is refused with a Greek
/// message and nothing is queued; the forms do not let it be typed.
#[tokio::test]
async fn a_story_number_longer_than_a_file_name_keeps_is_refused() -> Result<()> {
    use omni_core::slug::MAX_TYPED_INDEX;
    let app = App::new()?;
    let longest = "9".repeat(MAX_TYPED_INDEX);
    let (status, ok) = app
        .send("POST", "/api/jobs", &app.mcr_token, Some(json!({ "url": "https://example.test/idx/1", "keyword": "kw", "index_str": longest })))
        .await?;
    assert_eq!(status, StatusCode::OK, "{ok}");
    let (status, refused) = app
        .send("POST", "/api/jobs", &app.mcr_token, Some(json!({ "url": "https://example.test/idx/2", "keyword": "kw", "index_str": format!("{longest}9") })))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(refused.to_string().contains("έως 9 χαρακτήρες"), "{refused}");
    assert!(!app.repo.get_all_jobs()?.iter().any(|j| j.url == "https://example.test/idx/2"), "a refused link was queued");

    for page in ["mcr.html", "user.html"] {
        let html = omni_web::assets::WebAssets::get(page).expect("page shipped");
        let html = String::from_utf8_lossy(&html.data).to_string();
        assert!(html.contains(&format!("required maxlength=\"{MAX_TYPED_INDEX}\"")), "{page}: the story number has no limit");
    }
    Ok(())
}

/// P12.11: the forms' live preview promises exactly the name the job then gets, Greek and
/// signs included; for a rename it reads the keyword as the rename stores it.
#[tokio::test]
async fn the_file_name_preview_is_the_name_the_job_gets() -> Result<()> {
    let app = App::new()?;
    // Percent-encode every byte but letters and digits (Greek included).
    let q = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    for (n, (index, journalist, keyword)) in [
        ("2-B", "Papa-Daki", "fire in the hills"),
        ("1", "Παπαδάκη", "Σεισμός Ηράκλειο"),
        ("", "", ""),
        ("3", "MCR", &*"K".repeat(90)),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, preview) = app
            .send(
                "GET",
                &format!("/api/slug-preview?index={}&journalist={}&keyword={}", q(index), q(journalist), q(keyword)),
                &app.mcr_token,
                None,
            )
            .await?;
        assert_eq!(status, StatusCode::OK);
        let (status, created) = app
            .send(
                "POST",
                "/api/jobs",
                &app.mcr_token,
                Some(json!({ "url": format!("https://example.test/preview/{n}"), "keyword": keyword, "journalist": journalist, "index_str": index })),
            )
            .await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(preview["slug"], created["slug"], "{index}/{journalist}/{keyword}");
        assert!(preview["slug"].as_str().unwrap().len() <= omni_core::slug::MAX_LEN);
    }
    let (_, r) = app.send("GET", "/api/slug-preview?index=1&journalist=MCR&keyword=fire%20in&for=rename", &app.mcr_token, None).await?;
    assert_eq!(r["slug"], "1_MCR_FIREIN", "a rename's keyword keeps letters and digits only");
    let (_, r) = app.send("GET", "/api/slug-preview?index=1&journalist=MCR&keyword=x&for=rename", &app.mcr_token, None).await?;
    assert!(r["slug"].is_null(), "a rename of one letter promises a name");
    Ok(())
}

// ---- «Λάθος βίντεο» on a video delivered without approval (P10.16a) ----

/// A COMPLETED job whose latest repair attempt delivered without approval from a recipe
/// with 5 approvals on `news.example`, and the file it left in a stand-in watchfolder.
fn auto_delivered_job(app: &App, outcome: &str) -> Result<(i64, i64, String, std::path::PathBuf)> {
    use omni_core::recipe_score::{Event, InsertOutcome, NewRecipe, Origin, ScopeKind};
    let recipe = NewRecipe {
        scope_kind: ScopeKind::Site,
        scope_key: "news.example".into(),
        family: None,
        recipe_json: "{}".into(),
        hash: format!("hash-{outcome}"),
        steps_hash: format!("steps-{outcome}"),
        dsl_version: 1,
        origin: Origin::Admin,
    };
    let InsertOutcome::Inserted(rid) = app.repo.insert_recipe(&recipe, chrono::Utc::now())? else { panic!("recipe not stored") };
    for _ in 0..5 {
        app.repo.recipe_event(rid, "news.example", Event::Approval, chrono::Utc::now())?;
    }
    let id = app
        .repo
        .enqueue(
            &omni_core::models::NewJob::new("https://news.example/story-1", "1_NIKOLAOU_AUTO", "NIKOLAOU"),
            omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();
    app.repo.lease_job("test-worker", 180)?.expect("pending job leases");
    let at = app.repo.start_attempt(id, "hold", chrono::Utc::now())?;
    app.repo.finish_attempt(
        at,
        &omni_core::repository::AttemptEnd {
            rung: Some("recipes".into()),
            outcome: Some(outcome.into()),
            recipe_id: Some(rid),
            recipe_hash: Some(recipe.hash.clone()),
            domain: Some("news.example".into()),
            held_url: Some("https://cdn.news.example/v/a.mp4?token=abc".into()),
            ..Default::default()
        },
        chrono::Utc::now(),
    )?;
    let file = app._dir.path().join("watch").join("1_NIKOLAOU_AUTO.mxf");
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, b"on air")?;
    assert!(app.repo.finish(id, "test-worker", JobStatus::Completed, None, None, Some(&file.to_string_lossy()))?);
    Ok((id, rid, recipe.hash, file))
}

#[tokio::test]
async fn wrong_video_revokes_the_trust_once_and_never_touches_the_delivered_file() -> Result<()> {
    let app = App::new()?;
    let (id, rid, hash, file) = auto_delivered_job(&app, "auto_delivered")?;

    // The card says so before MCR presses anything.
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    let card = &json["job"]["repair"];
    assert_eq!((card["auto_delivered"].clone(), card["recipe_approvals"].clone(), card["wrong_video"].clone()), (json!(true), json!(5), json!(false)), "{json}");
    assert_eq!(card["source_host"], "cdn.news.example");
    assert!(app.repo.recipe_trusted_on(rid, &hash, "news.example", 5)?);

    let (status, json) = app.send("POST", &format!("/api/jobs/{id}/wrong-video"), &app.mcr_token, None).await?;
    assert_eq!((status, json["already"].clone()), (StatusCode::OK, json!(false)), "{json}");
    assert!(!app.repo.recipe_trusted_on(rid, &hash, "news.example", 5)?, "trust is gone at once");
    assert_eq!(std::fs::read(&file)?, b"on air", "the delivered file is untouched");
    let job = app.repo.get_job(id)?.unwrap();
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.file_path.as_deref(), Some(file.to_string_lossy().as_ref()));
    assert!(app.repo.get_recent_audit_logs(5)?.iter().any(|a| a.message.contains(&format!("Job #{id}")) && a.message.contains("wrong video")));
    assert!(app.repo.get_job_events(id, 50)?.iter().any(|e| e.message.contains("Dalet")));

    // Pressed again, nothing more happens; the card now says it was pressed.
    let events_before = app.repo.get_job_events(id, 50)?.len();
    let (status, json) = app.send("POST", &format!("/api/jobs/{id}/wrong-video"), &app.admin_token, None).await?;
    assert_eq!((status, json["already"].clone()), (StatusCode::OK, json!(true)), "{json}");
    assert_eq!(app.repo.get_job_events(id, 50)?.len(), events_before);
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(json["job"]["repair"]["wrong_video"], true, "{json}");
    Ok(())
}

#[tokio::test]
async fn wrong_video_on_a_job_nobody_delivered_automatically_is_a_conflict() -> Result<()> {
    let app = App::new()?;
    let (id, rid, hash, file) = auto_delivered_job(&app, "held")?;
    let (status, json) = app.send("POST", &format!("/api/jobs/{id}/wrong-video"), &app.mcr_token, None).await?;
    assert_eq!((status, json["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("NOT_AUTO_DELIVERED")), "{json}");
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert!(json["job"]["repair"].is_null(), "no auto-delivery card: {json}");
    assert!(app.repo.recipe_trusted_on(rid, &hash, "news.example", 5)?, "nothing was learned");
    assert_eq!(std::fs::read(&file)?, b"on air");
    // A job that does not exist.
    let (status, _) = app.send("POST", "/api/jobs/9999/wrong-video", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    // A plain user is refused.
    let (status, _) = app.send("POST", &format!("/api/jobs/{id}/wrong-video"), &app.user_token, None).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn a_completed_job_marked_auto_delivering_gets_the_card_and_wrong_video_works() -> Result<()> {
    // A crash after the delivery left the marker the ladder wrote before the pipeline ran (P10.16b).
    let app = App::new()?;
    let (id, rid, hash, file) = auto_delivered_job(&app, "auto_delivering")?;
    let (_, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    let card = &json["job"]["repair"];
    assert_eq!((card["auto_delivered"].clone(), card["wrong_video"].clone()), (json!(true), json!(false)), "{json}");
    let (status, json) = app.send("POST", &format!("/api/jobs/{id}/wrong-video"), &app.mcr_token, None).await?;
    assert_eq!((status, json["already"].clone()), (StatusCode::OK, json!(false)), "{json}");
    assert!(!app.repo.recipe_trusted_on(rid, &hash, "news.example", 5)?);
    assert_eq!(std::fs::read(&file)?, b"on air");
    Ok(())
}

/// P11.4 review round 1: a RATE_LIMITED job on a platform that ran out of
/// retries shows the exhausted text followed by the site's cookie advice (the
/// helper the worker's history line uses too); one still queued keeps the
/// code's own «Γίνεται αυτόματη επανάληψη».
#[tokio::test]
async fn a_rate_limit_that_ran_out_of_retries_on_a_platform_shows_the_cookie_advice() -> Result<()> {
    use omni_broadcast::errors::{exhausted_hint_el, login_hint_el, ErrorCode};
    let app = App::new()?;
    let new = |url: &str, slug: &str| {
        app.repo
            .enqueue(&omni_core::models::NewJob::new(url, slug, "NIKOLAOU"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
            .map(|e| e.job_id())
    };
    let id = new("https://x.com/someone/status/1", "1_NIKOLAOU_POST")?;
    for _ in 0..4 {
        assert_eq!(app.repo.lease_job("W", 180)?.unwrap().id, id);
        assert!(app.repo.requeue_after(id, "W", chrono::Duration::seconds(-1), "RATE_LIMITED", "limited")?);
    }
    assert_eq!(app.repo.lease_job("W", 180)?.unwrap().id, id);
    assert!(app.repo.finish(id, "W", JobStatus::RequiresReview, Some("RATE_LIMITED"), Some("limited"), None)?);
    let (status, json) = app.send("GET", &format!("/api/jobs/{id}"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["job"]["hint"], json!(format!("{} {}", exhausted_hint_el(5), login_hint_el("x.com", false))));

    let waiting = new("https://x.com/someone/status/2", "2_NIKOLAOU_POST")?;
    assert_eq!(app.repo.lease_job("W", 180)?.unwrap().id, waiting);
    assert!(app.repo.requeue_after(waiting, "W", chrono::Duration::minutes(2), "RATE_LIMITED", "limited")?);
    let (_, json) = app.send("GET", &format!("/api/jobs/{waiting}"), &app.mcr_token, None).await?;
    assert_eq!(json["job"]["hint"], json!(ErrorCode::RateLimited.hint_el()));
    Ok(())
}

// ---- P11.3b: late videos on the desk, and «Τώρα» ----

fn sql(app: &App, query: &str, params: impl rusqlite::Params) {
    rusqlite::Connection::open(app._dir.path().join("omni.db")).unwrap().execute(query, params).unwrap();
}

/// A PENDING job `mins` old.
fn aged_job(app: &App, n: u32, mins: i64) -> Result<i64> {
    let new = omni_core::models::NewJob::new(format!("https://example.gr/late{n}"), format!("{n}_ANNA_LATE"), "ANNA");
    let id = app.repo.enqueue(&new, 24)?.job_id();
    let at = omni_core::timestamps::format(chrono::Utc::now() - chrono::Duration::minutes(mins));
    sql(app, "UPDATE queue SET created_at = ? WHERE id = ?", rusqlite::params![at, id]);
    Ok(id)
}

#[tokio::test]
async fn the_desk_says_what_a_late_or_waiting_video_waits_for() -> Result<()> {
    let app = App::new()?;
    let queued = aged_job(&app, 1, 12)?;
    let backoff = aged_job(&app, 2, 30)?;
    let parked = aged_job(&app, 3, 20)?;
    let running = aged_job(&app, 4, 15)?;
    let young = aged_job(&app, 5, 1)?;
    let done = aged_job(&app, 6, 60)?;

    // Lease order is by priority then age; take them by id instead of relying on it.
    sql(&app, "UPDATE queue SET status = 'RUNNING', lease_owner = 'W' WHERE id = ?", rusqlite::params![running]);
    let later = omni_core::timestamps::format(chrono::Utc::now() + chrono::Duration::minutes(5));
    sql(&app, "UPDATE queue SET not_before = ?, error_code = 'NETWORK' WHERE id = ?", rusqlite::params![later, backoff]);
    sql(&app, "UPDATE queue SET not_before = ?, delivery_wait = 1 WHERE id = ?", rusqlite::params![later, parked]);
    sql(&app, "UPDATE queue SET status = 'COMPLETED' WHERE id = ?", rusqlite::params![done]);

    let (status, page) = app.send("GET", "/api/jobs?view=live", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{page}");
    let find = |id: i64| page["jobs"].as_array().unwrap().iter().find(|j| j["id"] == id).unwrap().clone();
    let facts = |id: i64| {
        let j = find(id);
        (j["late"].clone(), j["wait_reason"].clone(), j["next_try_at"].is_null())
    };
    assert_eq!(facts(queued), (json!(true), json!("queue"), true));
    assert_eq!(facts(backoff), (json!(true), json!("retry"), false));
    assert_eq!(facts(parked), (json!(true), json!("delivery"), false), "the watchfolder wait wins over the back-off");
    assert_eq!(facts(running), (json!(true), json!("running"), true));
    assert_eq!(facts(young), (json!(false), json!("queue"), true));
    assert_eq!(facts(done), (json!(false), json!(null), true));
    assert!(find(backoff)["next_try_at"].as_str().unwrap().ends_with('Z'));
    Ok(())
}

/// P11.7: a stream still live that waits for its next check is late from the
/// first check, says `live` with the check's time, and is in the late list
/// and its count, though younger than the late limit.
#[tokio::test]
async fn a_live_stream_waiting_for_its_next_check_is_late_and_says_live() -> Result<()> {
    let app = App::new()?;
    let live = aged_job(&app, 1, 2)?;
    let young = aged_job(&app, 2, 2)?;
    let later = omni_core::timestamps::format(chrono::Utc::now() + chrono::Duration::minutes(10));
    sql(&app, "UPDATE queue SET not_before = ?, error_code = 'LIVE_STREAM' WHERE id = ?", rusqlite::params![later, live]);
    sql(&app, "UPDATE queue SET not_before = ?, error_code = 'NETWORK' WHERE id = ?", rusqlite::params![later, young]);

    let (status, page) = app.send("GET", "/api/jobs?view=live", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{page}");
    let find = |id: i64| page["jobs"].as_array().unwrap().iter().find(|j| j["id"] == id).unwrap().clone();
    assert_eq!((find(live)["late"].clone(), find(live)["wait_reason"].clone()), (json!(true), json!("live")), "{page}");
    assert_eq!(find(live)["next_try_at"].as_str(), Some(later.as_str()));
    assert_eq!((find(young)["late"].clone(), find(young)["wait_reason"].clone()), (json!(false), json!("retry")));

    let (status, page) = app.send("GET", "/api/jobs?view=late", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ids: Vec<i64> = page["jobs"].as_array().unwrap().iter().map(|j| j["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![live], "{page}");
    assert_eq!(page["counts"]["late"], 1);
    assert_eq!(page["jobs"][0]["wait_reason"], "live");
    let (_, st) = app.send("GET", "/api/system/status", &app.mcr_token, None).await?;
    assert_eq!(st["queue"]["late"], 1, "{st}");
    Ok(())
}

#[tokio::test]
async fn the_late_view_lists_the_late_jobs_oldest_first_and_the_counts_say_how_many() -> Result<()> {
    let app = App::new()?;
    let newer = aged_job(&app, 1, 12)?;
    let older = aged_job(&app, 2, 50)?;
    let _young = aged_job(&app, 3, 2)?;
    let urgent = aged_job(&app, 4, 6)?;
    sql(&app, "UPDATE queue SET priority = ? WHERE id = ?", rusqlite::params![15, urgent]);

    let (status, page) = app.send("GET", "/api/jobs?view=late", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ids: Vec<i64> = page["jobs"].as_array().unwrap().iter().map(|j| j["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![older, newer, urgent], "{page}");
    assert_eq!(page["counts"]["late"], 3);
    assert!(page["jobs"][0]["late"].as_bool().unwrap());

    // The status payload the desk polls carries the same number.
    let (status, st) = app.send("GET", "/api/system/status", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(st["queue"]["late"], 3, "{st}");

    // The limits are the live config's: a user is not allowed here at all.
    let (status, _) = app.send("GET", "/api/jobs?view=late", &app.user_token, None).await?;
    assert_ne!(status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn run_now_ends_a_back_off_once_and_leaves_the_audit_trail() -> Result<()> {
    let app = App::new()?;
    let id = aged_job(&app, 1, 12)?;
    let later = omni_core::timestamps::format(chrono::Utc::now() + chrono::Duration::minutes(5));
    sql(&app, "UPDATE queue SET not_before = ? WHERE id = ?", rusqlite::params![later, id]);

    let (status, json) = app.send("POST", &format!("/api/jobs/{id}/now"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(app.repo.get_job(id)?.unwrap().not_before.is_none());
    assert!(app.repo.get_job_events(id, 50)?.iter().any(|e| e.message.starts_with("Run now requested by")));
    assert!(app.repo.get_recent_audit_logs(5)?.iter().any(|a| a.message.contains(&format!("Job #{id}")) && a.message.contains("run now")));

    // Pressed again: nothing to clear, so a conflict with the project's error shape.
    let events = app.repo.get_job_events(id, 50)?.len();
    let (status, json) = app.send("POST", &format!("/api/jobs/{id}/now"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "NOT_WAITING_TO_RETRY", "{json}");
    assert_eq!(app.repo.get_job_events(id, 50)?.len(), events, "a refused click records nothing");

    // A video waiting for the watchfolder is not released by the click.
    let parked = aged_job(&app, 2, 12)?;
    sql(&app, "UPDATE queue SET not_before = ?, delivery_wait = 1 WHERE id = ?", rusqlite::params![later, parked]);
    let (status, _) = app.send("POST", &format!("/api/jobs/{parked}/now"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(app.repo.get_job(parked)?.unwrap().not_before.is_some());
    Ok(())
}

#[tokio::test]
async fn the_late_limits_are_the_configured_ones() -> Result<()> {
    let app = App::with_config(|c| {
        c.retry.late_after_mins = 30;
        c.retry.late_after_urgent_mins = 20;
    })?;
    let young = aged_job(&app, 1, 12)?;
    let old = aged_job(&app, 2, 31)?;

    let (_, page) = app.send("GET", "/api/jobs?view=late", &app.mcr_token, None).await?;
    let ids: Vec<i64> = page["jobs"].as_array().unwrap().iter().map(|j| j["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![old], "{page}");
    assert_eq!(page["counts"]["late"], 1);

    let (_, live) = app.send("GET", "/api/jobs?view=live", &app.mcr_token, None).await?;
    let late_of = |id: i64| live["jobs"].as_array().unwrap().iter().find(|j| j["id"] == id).unwrap()["late"].clone();
    assert_eq!((late_of(young), late_of(old)), (json!(false), json!(true)), "{live}");

    let (_, st) = app.send("GET", "/api/system/status", &app.mcr_token, None).await?;
    assert_eq!(st["queue"]["late"], 1, "{st}");
    Ok(())
}

/// P12.4: the inline rename field's live preview names the file the rename then gives. Same
/// keyword (Greek transliterated, upper case, letters and digits), and nothing under two of them.
#[tokio::test]
async fn the_keyword_preview_is_the_keyword_the_rename_stores() -> Result<()> {
    let app = App::new()?;
    let (status, created) = app
        .send("POST", "/api/jobs", &app.mcr_token, Some(json!({ "url": "https://www.youtube.com/watch?v=preview1" })))
        .await?;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["job_id"].as_i64().unwrap();
    // Percent-encode every byte that is not a plain letter or digit.
    let encode = |s: &str| -> String {
        s.bytes()
            .map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect()
    };

    for (typed, want) in [("σεισμός σητεία", Some("SEISMOSSITEIA")), ("Fire-Heraklion 2", Some("FIREHERAKLION2")), ("ά!", None)] {
        let (status, preview) = app
            .send("GET", &format!("/api/keyword-preview?text={}", encode(typed)), &app.mcr_token, None)
            .await?;
        assert_eq!(status, StatusCode::OK, "{preview}");
        assert_eq!(preview["keyword"].as_str(), want, "preview of {typed:?}");
        let (status, renamed) = app
            .send("POST", &format!("/api/jobs/{id}/rename"), &app.mcr_token, Some(json!({ "keyword": typed })))
            .await?;
        match want {
            Some(k) => {
                assert_eq!(status, StatusCode::OK, "{renamed}");
                assert_eq!(renamed["keyword"], k, "the rename stored another keyword than the preview showed");
                assert!(renamed["slug"].as_str().unwrap().ends_with(&format!("_{k}")), "{renamed}");
            }
            None => assert_eq!(status, StatusCode::BAD_REQUEST, "{renamed}"),
        }
    }
    // A long paste is cut, never refused.
    let long = "a".repeat(5000);
    let (status, preview) = app.send("GET", &format!("/api/keyword-preview?text={long}"), &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview["keyword"].as_str().map(str::len), Some(20));
    Ok(())
}
