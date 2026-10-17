//! The journalist panel API (plan P7.3): submit, cancel own, preference, and
//! the shape of `/api/jobs/mine`. Names and addresses are invented.

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

struct Panel {
    router: axum::Router,
    repo: Repository,
    ann_id: i64,
    ann: String,
    ben_id: i64,
    ben: String,
    dir: TempDir,
}

impl Panel {
    fn new() -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let login =|email: &str, surname: &str| -> Result<(i64, String)> {
            let id = repo.create_user(email, "correct-horse-battery", UserRole::User, "Tester", Some(surname))?;
            let token = omni_core::auth::generate_session_token();
            repo.create_session(id, &token, 1)?;
            Ok((id, token))
        };
        let (ann_id, ann) = login("ann@example.test", "ANNOU")?;
        let (ben_id, ben) = login("ben@example.test", "BENOU")?;
        let state = AppState::new(repo.clone(), AppConfig::default(), dir.path().join("config.json"));
        let router = WebServer::build_router(state);
        Ok(Self { router, repo, ann_id, ann, ben_id, ben, dir })
    }

    async fn send(&self, method: &str, uri: &str, token: &str, body: Option<Value>) -> Result<(StatusCode, Value)> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", format!("omni_session={token}"))
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
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }

    fn job_count(&self) -> usize {
        self.repo.get_all_jobs().unwrap().len()
    }
}

#[tokio::test]
async fn a_plain_user_submits_as_themselves_while_the_mcr_route_stays_closed_to_them() -> Result<()> {
    let p = Panel::new()?;
    let (status, json) = p
        .send(
            "POST",
            "/api/user/jobs",
            &p.ann,
            Some(json!({"url": "https://example.test/v/1", "keyword": "Σεισμός", "journalist": "SOMEONEELSE"})),
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    let id = json[0]["job_id"].as_i64().unwrap();
    assert_eq!(json[0]["duplicate"], false);
    let job = p.repo.get_job(id)?.unwrap();
    // The account's surname, whatever the payload claims.
    assert_eq!(job.journalist, "ANNOU");
    assert_eq!(job.submitted_by_user_id, Some(p.ann_id));
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!(job.priority, 0);
    assert_eq!(job.slug, "1_ANNOU_SEISMOS");

    // Negative control: the MCR route is still not theirs.
    let (status, _) = p
        .send("POST", "/api/jobs", &p.ann, Some(json!({"url": "https://example.test/v/2"})))
        .await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(p.job_count(), 1);
    Ok(())
}

#[tokio::test]
async fn many_links_get_index_suffixes_and_one_bad_link_queues_nothing() -> Result<()> {
    let p = Panel::new()?;
    let urls: Vec<String> = (1..=3).map(|n| format!("https://example.test/v/{n}")).collect();
    let (status, json) = p
        .send("POST", "/api/user/jobs", &p.ann, Some(json!({"urls": urls, "keyword": "FOTIA"})))
        .await?;
    assert_eq!(status, StatusCode::OK);
    let slugs: Vec<&str> = json.as_array().unwrap().iter().map(|r| r["slug"].as_str().unwrap()).collect();
    assert_eq!(slugs, ["1A_ANNOU_FOTIA", "1B_ANNOU_FOTIA", "1C_ANNOU_FOTIA"]);
    assert_eq!(p.job_count(), 3);

    let (status, _) = p
        .send(
            "POST",
            "/api/user/jobs",
            &p.ben,
            Some(json!({"urls": ["https://example.test/ok", "javascript:alert(1)"], "keyword": "X"})),
        )
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(p.job_count(), 3, "a list with one bad link must queue nothing");

    let many: Vec<String> = (0..21).map(|n| format!("https://example.test/m/{n}")).collect();
    let (status, _) = p
        .send("POST", "/api/user/jobs", &p.ben, Some(json!({"urls": many, "keyword": "X"})))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(p.job_count(), 3);

    // Exactly one of url / urls.
    let (status, _) = p
        .send("POST", "/api/user/jobs", &p.ben, Some(json!({"url": "https://example.test/a", "urls": ["https://example.test/b"], "keyword": "X"})))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn the_same_link_twice_is_a_duplicate_and_makes_no_second_job() -> Result<()> {
    let p = Panel::new()?;
    let body = json!({"url": "https://example.test/v/dup", "keyword": "DUP"});
    let (_, first) = p.send("POST", "/api/user/jobs", &p.ann, Some(body.clone())).await?;
    let (status, second) = p.send("POST", "/api/user/jobs", &p.ann, Some(body)).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second[0]["duplicate"], true);
    assert_eq!(second[0]["job_id"], first[0]["job_id"]);
    assert_eq!(p.job_count(), 1);
    Ok(())
}

#[tokio::test]
async fn urgent_is_priority_ten_and_never_more() -> Result<()> {
    let p = Panel::new()?;
    let (_, urgent) = p
        .send("POST", "/api/user/jobs", &p.ann, Some(json!({"url": "https://example.test/u", "keyword": "U", "priority": true})))
        .await?;
    assert_eq!(p.repo.get_job(urgent[0]["job_id"].as_i64().unwrap())?.unwrap().priority, 10);
    // A number is not a yes/no: refused, nothing queued, no way to ask for 100.
    let (status, _) = p
        .send("POST", "/api/user/jobs", &p.ann, Some(json!({"url": "https://example.test/h", "keyword": "H", "priority": 100})))
        .await?;
    assert!(status.is_client_error());
    assert_eq!(p.job_count(), 1);
    Ok(())
}

#[tokio::test]
async fn cancel_is_for_the_owner_of_a_pending_job_only() -> Result<()> {
    let p = Panel::new()?;
    let (_, made) = p
        .send("POST", "/api/user/jobs", &p.ann, Some(json!({"url": "https://example.test/c", "keyword": "C"})))
        .await?;
    let id = made[0]["job_id"].as_i64().unwrap();

    // Another journalist: 404, and the job is untouched.
    let (status, _) = p.send("POST", &format!("/api/user/jobs/{id}/cancel"), &p.ben, Some(json!({}))).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(p.repo.get_job(id)?.unwrap().status, JobStatus::Pending);
    let (status, _) = p.send("POST", "/api/user/jobs/987654/cancel", &p.ben, Some(json!({}))).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The owner.
    let (status, _) = p.send("POST", &format!("/api/user/jobs/{id}/cancel"), &p.ann, Some(json!({}))).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(p.repo.get_job(id)?.unwrap().status, JobStatus::Cancelled);
    let events = p.repo.get_job_events(id, 10)?;
    assert!(events.iter().any(|e| e.message.contains("Cancelled")), "no cancel event: {events:?}");
    Ok(())
}

#[tokio::test]
async fn cancel_refuses_own_jobs_that_are_running_or_done() -> Result<()> {
    let p = Panel::new()?;
    for (tag, status_name) in [("RUN", "RUNNING"), ("DONE", "COMPLETED")] {
        let (_, made) = p
            .send(
                "POST",
                "/api/user/jobs",
                &p.ann,
                Some(json!({"url": format!("https://example.test/{tag}"), "keyword": tag})),
            )
            .await?;
        let id = made[0]["job_id"].as_i64().unwrap();
        // Move the job on the way a worker would.
        match status_name {
            "RUNNING" => {
                let leased = p.repo.lease_job("test-worker", 60)?.expect("a job to lease");
                assert_eq!(leased.id, id);
            }
            _ => {
                let leased = p.repo.lease_job("test-worker", 60)?.expect("a job to lease");
                assert_eq!(leased.id, id);
                assert!(p.repo.finish(id, "test-worker", JobStatus::Completed, None, None, Some("out.mxf"))?);
            }
        }
        let (status, json) = p.send("POST", &format!("/api/user/jobs/{id}/cancel"), &p.ann, Some(json!({}))).await?;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["error"]["code"], "NOT_CANCELLABLE");
        assert_eq!(p.repo.get_job(id)?.unwrap().status.as_str(), status_name);
    }
    Ok(())
}

#[tokio::test]
async fn the_notify_preference_round_trips_per_account() -> Result<()> {
    let p = Panel::new()?;
    let (_, json) = p.send("GET", "/api/user/prefs", &p.ann, None).await?;
    assert_eq!(json["notify_on_completion"], false);
    let (status, _) = p.send("PUT", "/api/user/prefs", &p.ann, Some(json!({"notify_on_completion": true}))).await?;
    assert_eq!(status, StatusCode::OK);
    let (_, json) = p.send("GET", "/api/user/prefs", &p.ann, None).await?;
    assert_eq!(json["notify_on_completion"], true);
    let (_, json) = p.send("GET", "/api/user/prefs", &p.ben, None).await?;
    assert_eq!(json["notify_on_completion"], false, "Ann's choice leaked to Ben");
    assert_eq!(p.repo.get_notify_on_completion(p.ben_id)?, Some(false));
    Ok(())
}

#[tokio::test]
async fn my_jobs_shows_the_file_name_but_no_server_paths() -> Result<()> {
    let p = Panel::new()?;
    let (_, made) = p
        .send("POST", "/api/user/jobs", &p.ann, Some(json!({"url": "https://example.test/f", "keyword": "F"})))
        .await?;
    let id = made[0]["job_id"].as_i64().unwrap();
    p.repo.lease_job("test-worker", 60)?.expect("a job to lease");
    let delivered = r"\\dalet\ingest\1_ANNOU_F.mxf";
    assert!(p.repo.finish(id, "test-worker", JobStatus::Completed, None, None, Some(delivered))?);

    let (status, json) = p.send("GET", "/api/jobs/mine", &p.ann, None).await?;
    assert_eq!(status, StatusCode::OK);
    let job = &json["jobs"][0];
    assert_eq!(job["id"].as_i64(), Some(id));
    assert!(job.get("file_path").is_none(), "file_path leaked: {job}");
    assert!(job.get("source_path").is_none(), "source_path leaked: {job}");
    assert_eq!(job["delivered_name"], "1_ANNOU_F.mxf");
    assert!(!json.to_string().contains("dalet"), "a directory leaked: {json}");
    // Another account sees none of it.
    let (_, json) = p.send("GET", "/api/jobs/mine", &p.ben, None).await?;
    assert!(json["jobs"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn a_duplicate_of_someone_elses_job_does_not_name_it() -> Result<()> {
    let p = Panel::new()?;
    let body = json!({"url": "https://example.test/v/shared", "keyword": "SECRETKW"});
    let (_, ben_made) = p.send("POST", "/api/user/jobs", &p.ben, Some(body.clone())).await?;
    let ben_job = ben_made[0]["job_id"].as_i64().unwrap();

    let (status, ann_res) = p.send("POST", "/api/user/jobs", &p.ann, Some(body.clone())).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ann_res[0]["duplicate"], true);
    assert!(ann_res[0]["job_id"].is_null() && ann_res[0]["slug"].is_null(), "leaked: {ann_res}");
    let text = ann_res.to_string();
    assert!(!text.contains("BENOU") && !text.contains("SECRETKW"), "leaked: {text}");
    assert_eq!(p.job_count(), 1);

    // Control: the owner resubmitting gets their own job back.
    let (_, again) = p.send("POST", "/api/user/jobs", &p.ben, Some(body)).await?;
    assert_eq!(again[0]["duplicate"], true);
    assert_eq!(again[0]["job_id"].as_i64(), Some(ben_job));
    assert_eq!(again[0]["slug"], "1_BENOU_SECRETKW");
    Ok(())
}

#[tokio::test]
async fn a_roster_journalist_keeps_their_dash_in_the_identity_() -> Result<()> {
    let p = Panel::new()?;
    let id = p.repo.create_user("pd@example.test", "correct-horse-battery", UserRole::User, "Tester", Some("PAPA-DAKI"))?;
    let token = omni_core::auth::generate_session_token();
    p.repo.create_session(id, &token, 1)?;
    p.repo.save_journalist("PAPA-DAKI", "Tester Papa-Daki", &[], 7)?;

    let (status, r) = p
        .send("POST", "/api/user/jobs", &token, Some(json!({"url": "https://example.test/pd/1", "keyword": "fire-in-hills"})))
        .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(r[0]["slug"], "1_PAPA-DAKI_FIRE-IN-HILLS");
    let job = p.repo.get_job(r[0]["job_id"].as_i64().unwrap())?.unwrap();
    // The identity still matches the roster, so the journalist's default
    // priority (7) is applied by enqueue.
    assert_eq!(job.journalist, "PAPA-DAKI");
    assert_eq!(job.priority, 7);
    Ok(())
}

#[tokio::test]
async fn a_failure_part_way_through_a_list_is_reported_with_what_was_done() -> Result<()> {
    let p = Panel::new()?;
    // A trigger makes the insert of the marked link fail with a message that
    // names a path; none of it may reach the response.
    let conn = rusqlite::Connection::open(p.dir.path().join("omni.db"))?;
    conn.execute_batch(
        "CREATE TRIGGER fail_marked BEFORE INSERT ON queue WHEN NEW.url LIKE '%FAILME%'
         BEGIN SELECT RAISE(ABORT, 'boom C:/secret/omni.db'); END;",
    )?;
    drop(conn);

    let (status, json) = p
        .send(
            "POST",
            "/api/user/jobs",
            &p.ann,
            Some(json!({"urls": ["https://example.test/a", "https://example.test/FAILME", "https://example.test/c"], "keyword": "K"})),
        )
        .await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json["error"]["code"], "PARTIAL_FAILURE");
    let results = json["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["duplicate"], false);
    assert!(results[0]["job_id"].as_i64().is_some());
    assert!(results[1]["error"].is_string() && results[1]["job_id"].is_null());
    assert_eq!(json["not_attempted"], 1);
    assert_eq!(p.job_count(), 1, "only the first link was queued");
    let text = json.to_string().to_lowercase();
    assert!(!text.contains("boom") && !text.contains("secret") && !text.contains("sqlite"), "leaked: {text}");
    Ok(())
}

#[tokio::test]
async fn a_user_jobs_slug_is_built_from_its_stored_fields() -> Result<()> {
    let p = Panel::new()?;
    let urls: Vec<String> = (1..=2).map(|n| format!("https://example.test/inv/{n}")).collect();
    for body in [
        json!({"url": "https://example.test/inv/0", "keyword": "Σεισμός Ηράκλειο", "index_str": "3-b"}),
        json!({"urls": urls, "keyword": "fire-in-hills"}),
    ] {
        let (status, r) = p.send("POST", "/api/user/jobs", &p.ann, Some(body)).await?;
        assert_eq!(status, StatusCode::OK);
        for item in r.as_array().unwrap() {
            let job = p.repo.get_job(item["job_id"].as_i64().unwrap())?.unwrap();
            // What article.rs `house_slug` compares against.
            assert_eq!(job.slug, format!("{}_{}_{}", job.index_str, job.journalist, job.keyword));
        }
    }
    Ok(())
}
