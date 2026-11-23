//! The admin's Cookies card routes (plan P3.5): what they refuse, what they
//! store, and that no response, audit line or list ever carries a cookie value.
//! The yt-dlp test itself is the daemon's; here it is a fake. Who may call is in
//! `auth_matrix_tests.rs`. Invented sites and values; no network.

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::health::{checks, Health, HealthState};
use omni_core::logging::Redactions;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::{AppState, CookieTestFuture, CookieTester};

const PEER: &str = "127.0.0.1:50000";
const VALUE: &str = "known-cookie-value-7f3a9c";
const OTHER_VALUE: &str = "second-cookie-value-51b2d8";

struct FakeTester {
    answer: Mutex<Result<(bool, String), String>>,
    asked: Mutex<Vec<(String, String)>>,
}

impl CookieTester for FakeTester {
    fn test<'a>(&'a self, domain: &'a str, url: &'a str) -> CookieTestFuture<'a> {
        Box::pin(async move {
            self.asked.lock().unwrap().push((domain.to_string(), url.to_string()));
            self.answer.lock().unwrap().clone()
        })
    }
}

struct App {
    router: axum::Router,
    repo: Repository,
    health: HealthState,
    redactions: Redactions,
    admin: String,
    _dir: TempDir,
}

impl App {
    fn new(tester: Option<Arc<FakeTester>>) -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let id = repo.create_user("it@station.gr", "correct-horse-battery", UserRole::Admin, "Administrator", None)?;
        let admin = omni_core::auth::generate_session_token();
        repo.create_session(id, &admin, 1)?;
        let health = HealthState::new();
        let redactions = Redactions::default();
        let mut state = AppState::new(repo.clone(), AppConfig::default(), dir.path().join("config.json"))
            .with_health(health.clone())
            .with_redactions(redactions.clone());
        if let Some(t) = tester {
            state = state.with_cookie_tester(t);
        }
        Ok(Self { router: WebServer::build_router(state), repo, health, redactions, admin, _dir: dir })
    }

    /// Status and the raw body text (cookie values are asserted on the text).
    async fn send(&self, method: &str, uri: &str, body: Option<Value>, marker: bool) -> Result<(StatusCode, String)> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", format!("omni_session={}", self.admin))
            .header("host", "mcr.local")
            .header("origin", "http://mcr.local");
        if marker {
            b = b.header("x-omni-request", "1");
        }
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v)?)
            }
            None => Body::empty(),
        };
        let res = self.router.clone().layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap())).oneshot(b.body(body)?).await?;
        let status = res.status();
        let bytes = res.into_body().collect().await?.to_bytes();
        Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
    }

    async fn upload(&self, domain: &str, netscape: &str) -> Result<(StatusCode, String)> {
        self.send("POST", "/api/admin/cookies", Some(json!({ "domain": domain, "netscape": netscape })), true).await
    }

    async fn audit(&self) -> Result<String> {
        Ok(self.repo.get_recent_audit_logs(50)?.into_iter().map(|a| a.message).collect::<Vec<_>>().join("\n"))
    }
}

fn jar_text(value: &str, expires: i64) -> String {
    format!(
        "# Netscape HTTP Cookie File\n.x.com\tTRUE\t/\tTRUE\t{expires}\tauth_token\t{value}\n#HttpOnly_.x.com\tTRUE\t/\tTRUE\t{expires}\tct0\t{value}\n.example.org\tTRUE\t/\tFALSE\t0\tother\t{OTHER_VALUE}\n"
    )
}

fn tester(answer: Result<(bool, String), String>) -> Arc<FakeTester> {
    Arc::new(FakeTester { answer: Mutex::new(answer), asked: Mutex::new(Vec::new()) })
}

#[tokio::test]
async fn an_upload_is_stored_and_nothing_that_comes_back_holds_a_cookie_value() -> Result<()> {
    let app = App::new(None)?;
    let (status, body) = app.upload("x.com", &jar_text(VALUE, 4_102_444_800)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let j: Value = serde_json::from_str(&body)?;
    assert_eq!((j["cookie_count"].as_i64(), j["skipped_other_site"].as_i64()), (Some(2), Some(1)));
    assert!(!body.contains(VALUE) && !body.contains(OTHER_VALUE), "the upload response echoes cookies: {body}");

    let (status, list) = app.send("GET", "/api/admin/cookies", None, false).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(!list.contains(VALUE) && !list.contains("auth_token") && !list.contains("ct0"), "the list shows contents: {list}");
    let l: Value = serde_json::from_str(&list)?;
    assert_eq!(l["jars"][0]["domain"], "x.com");
    assert_eq!(l["jars"][0]["updated_by"], "it@station.gr");
    assert_eq!(l["jars"][0]["cookie_count"], 2);

    let audit = app.audit().await?;
    assert!(audit.contains("uploaded 2 cookie(s) for x.com"), "{audit}");
    assert!(!audit.contains(VALUE), "{audit}");

    // The value is stored (the downloader reads it back) and a foreign site's cookie is not.
    let stored = app.repo.cookie_jar_text("x.com")?.unwrap();
    assert!(stored.contains(VALUE) && !stored.contains(OTHER_VALUE));
    // ...and from this moment it is redacted from the log.
    assert_eq!(app.redactions.apply(&format!("yt-dlp said {VALUE}")), "yt-dlp said ***");
    Ok(())
}

#[tokio::test]
async fn a_replacement_keeps_one_jar_and_twitter_com_is_the_x_jar() -> Result<()> {
    let app = App::new(None)?;
    app.upload("twitter.com", &jar_text(VALUE, 0)).await?;
    let (_, body) = app.upload("x.com", &jar_text("replacement-value-123456", 0)).await?;
    assert_eq!(serde_json::from_str::<Value>(&body)?["replaced"], true);
    let jars = app.repo.list_cookie_jars()?;
    assert_eq!(jars.len(), 1);
    assert_eq!(jars[0].domain, "x.com");
    assert!(!app.repo.cookie_jar_text("x.com")?.unwrap().contains(VALUE));
    Ok(())
}

#[tokio::test]
async fn a_bad_upload_is_refused_without_echoing_it_and_stores_nothing() -> Result<()> {
    let app = App::new(None)?;
    let foreign = format!(".example.org\tTRUE\t/\tFALSE\t0\tother\t{OTHER_VALUE}\n");
    let (status, body) = app.upload("x.com", &foreign).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!body.contains(OTHER_VALUE) && !body.contains("other\t"), "{body}");
    let (status, _) = app.upload("x.com", "").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = app.upload("x.com", &"a".repeat(300 * 1024)).await?;
    assert_ne!(status, StatusCode::OK);
    for domain in ["", "www.x.com", "x.com/path", "https://x.com", "127.0.0.1", "x"] {
        let (status, _) = app.upload(domain, &jar_text(VALUE, 0)).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{domain:?}");
    }
    assert!(app.repo.list_cookie_jars()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_write_without_the_csrf_marker_changes_nothing() -> Result<()> {
    let app = App::new(None)?;
    let body = json!({ "domain": "x.com", "netscape": jar_text(VALUE, 0) });
    let (status, _) = app.send("POST", "/api/admin/cookies", Some(body), false).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(app.repo.list_cookie_jars()?.is_empty());

    app.upload("x.com", &jar_text(VALUE, 0)).await?;
    let (status, _) = app.send("POST", "/api/admin/cookies/x.com/delete", None, false).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app.send("POST", "/api/admin/cookies/x.com/test", Some(json!({ "url": "https://x.com/a/status/1" })), false).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(app.repo.list_cookie_jars()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn delete_removes_the_jar_and_a_second_delete_is_refused() -> Result<()> {
    let app = App::new(None)?;
    app.upload("x.com", &jar_text(VALUE, 0)).await?;
    let (status, _) = app.send("POST", "/api/admin/cookies/x.com/delete", None, true).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(app.repo.list_cookie_jars()?.is_empty());
    let (status, _) = app.send("POST", "/api/admin/cookies/x.com/delete", None, true).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(app.audit().await?.contains("deleted the cookies for x.com"));
    Ok(())
}

#[tokio::test]
async fn the_test_runs_on_a_sample_of_the_site_and_reports_ok_or_the_first_error_line() -> Result<()> {
    let t = tester(Ok((false, "ERROR: login required".to_string())));
    let app = App::new(Some(t.clone()))?;
    app.upload("x.com", &jar_text(VALUE, 0)).await?;

    // Not on the site, no address, or private: the tester is never asked.
    for url in ["https://example.org/a", "http://127.0.0.1/a", "https://x.com.evil.example/a", "nonsense", ""] {
        let (status, _) = app.send("POST", "/api/admin/cookies/x.com/test", Some(json!({ "url": url })), true).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url:?}");
    }
    assert!(t.asked.lock().unwrap().is_empty());
    // A site with no jar.
    let (status, _) = app.send("POST", "/api/admin/cookies/facebook.com/test", Some(json!({ "url": "https://facebook.com/a" })), true).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = app
        .send("POST", "/api/admin/cookies/x.com/test", Some(json!({ "url": "https://x.com/a/status/1?s=20" })), true)
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let j: Value = serde_json::from_str(&body)?;
    assert_eq!((j["ok"].as_bool(), j["detail"].as_str()), (Some(false), Some("ERROR: login required")));
    assert_eq!(t.asked.lock().unwrap()[0].0, "x.com");
    let audit = app.audit().await?;
    assert!(audit.contains("tested the cookies for x.com on x.com/a/status/1"), "{audit}");
    assert!(!audit.contains("s=20"), "the audit line carries no query: {audit}");
    Ok(())
}

#[tokio::test]
async fn without_a_tester_the_test_is_unavailable() -> Result<()> {
    let app = App::new(None)?;
    app.upload("x.com", &jar_text(VALUE, 0)).await?;
    let (status, _) = app.send("POST", "/api/admin/cookies/x.com/test", Some(json!({ "url": "https://x.com/a/status/1" })), true).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
async fn the_sample_link_is_shown_as_host_and_path_only() -> Result<()> {
    let app = App::new(None)?;
    app.upload("x.com", &jar_text(VALUE, 0)).await?;
    app.repo.record_cookie_test("x.com", true, "ok", "https://x.com/a/status/1?s=20&t=zzz", chrono::Utc::now())?;
    let (_, list) = app.send("GET", "/api/admin/cookies", None, false).await?;
    let l: Value = serde_json::from_str(&list)?;
    assert_eq!(l["jars"][0]["sample"], "x.com/a/status/1");
    assert!(!list.contains("zzz") && !list.contains("s=20"), "{list}");
    Ok(())
}

#[tokio::test]
async fn the_cookies_health_line_follows_the_jars() -> Result<()> {
    let app = App::new(None)?;
    // An upload that expires in three days: amber at once.
    let soon = chrono::Utc::now().timestamp() + 3 * 86_400;
    app.upload("x.com", &jar_text(VALUE, soon)).await?;
    let line = app.health.get(checks::COOKIES).expect("a Cookies line");
    assert_eq!(line.state, Health::Degraded);
    assert!(line.detail.as_deref().unwrap_or("").contains("x.com"));
    // A long-lived jar: fine.
    app.upload("x.com", &jar_text(VALUE, 4_102_444_800)).await?;
    assert_eq!(app.health.get(checks::COOKIES).unwrap().state, Health::Ok);
    // Deleted: no jars, fine, and the line says so.
    app.send("POST", "/api/admin/cookies/x.com/delete", None, true).await?;
    assert_eq!(app.health.get(checks::COOKIES).unwrap().detail.as_deref(), Some("No cookie jars"));
    Ok(())
}

#[tokio::test]
async fn the_twenty_first_site_is_refused() -> Result<()> {
    let app = App::new(None)?;
    for i in 0..20 {
        let d = format!("site{i}.example");
        let text = format!(".{d}\tTRUE\t/\tTRUE\t0\tn\tvalue-{i}-123456\n");
        let (status, body) = app.upload(&d, &text).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, _) = app.upload("onemore.example", ".onemore.example\tTRUE\t/\tTRUE\t0\tn\tvalue-123456\n").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Replacing one of the twenty still works.
    let (status, _) = app.upload("site4.example", ".site4.example\tTRUE\t/\tTRUE\t0\tn\tnew-value-123456\n").await?;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

/// A job on `url` waiting in review with `code`, as its worker leaves it.
fn job_in_review(repo: &Repository, url: &str, slug: &str, code: &str) -> Result<i64> {
    use omni_core::models::{JobStatus, NewJob};
    let id = repo.enqueue(&NewJob::new(url.to_string(), slug.to_string(), "ANNA"), 24)?.job_id();
    assert!(repo.lease_job_by_id(id, "host:1:0", 180)?);
    assert!(repo.finish(id, "host:1:0", JobStatus::RequiresReview, Some(code), Some("login"), None)?);
    Ok(id)
}

fn status_of(repo: &Repository, id: i64) -> omni_core::models::JobStatus {
    repo.get_job(id).unwrap().unwrap().status
}

/// P11.5: a saved jar gives the site's login jobs in review a second chance;
/// another site's job and a job a person acted on stay.
#[tokio::test]
async fn an_upload_gives_the_sites_login_jobs_a_second_chance() -> Result<()> {
    use omni_core::models::JobStatus;
    let app = App::new(None)?;
    let login = job_in_review(&app.repo, "https://twitter.com/someone/status/1", "1_ANNA_TEST", "LOGIN_REQUIRED")?;
    let other = job_in_review(&app.repo, "https://www.instagram.com/p/abc", "2_ANNA_TEST", "LOGIN_REQUIRED")?;
    let touched = job_in_review(&app.repo, "https://x.com/someone/status/3", "3_ANNA_TEST", "LOGIN_REQUIRED")?;
    assert!(app.repo.retry_job(touched, None)?);
    assert!(app.repo.lease_job_by_id(touched, "host:1:0", 180)?);
    assert!(app.repo.finish(touched, "host:1:0", JobStatus::RequiresReview, Some("LOGIN_REQUIRED"), Some("login"), None)?);

    let (status, body) = app.upload("x.com", &jar_text(VALUE, 4_102_444_800)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(status_of(&app.repo, login), JobStatus::Pending);
    let causes = app.repo.second_chances(login)?;
    assert!(causes.len() == 1 && causes[0].starts_with("cookies:x.com:"), "{causes:?}");
    assert_eq!(status_of(&app.repo, other), JobStatus::RequiresReview, "another site's job moved");
    assert_eq!(status_of(&app.repo, touched), JobStatus::RequiresReview, "a job a person acted on moved");
    let audit = app.audit().await?;
    assert!(!audit.contains(VALUE), "{audit}");
    Ok(())
}

/// P11.5: a test that logs in gives a second chance; a failed one does not.
#[tokio::test]
async fn a_passing_test_gives_a_second_chance_and_a_failing_one_does_not() -> Result<()> {
    use omni_core::models::JobStatus;
    let failing = tester(Ok((false, "ERROR: login required".into())));
    let app = App::new(Some(failing.clone()))?;
    app.upload("x.com", &jar_text(VALUE, 4_102_444_800)).await?;
    // A job that went to review after the upload.
    let id = job_in_review(&app.repo, "https://x.com/someone/status/1", "1_ANNA_TEST", "LOGIN_REQUIRED")?;
    let test = || app.send("POST", "/api/admin/cookies/x.com/test", Some(json!({ "url": "https://x.com/a/status/1" })), true);
    let (status, _) = test().await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_of(&app.repo, id), JobStatus::RequiresReview, "a failing test moved a job");

    *failing.answer.lock().unwrap() = Ok((true, "ok".into()));
    let (status, _) = test().await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_of(&app.repo, id), JobStatus::Pending, "a passing test did not move the login job");
    Ok(())
}

/// P11.12: the desk card lists the login-wall jobs in review per site whose jar
/// is missing, expired or failed, the most first, with the newest job. A site
/// with a usable jar, a news article's player, another code and a tidied job
/// are left out; no address leaves the server.
#[tokio::test]
async fn the_desk_card_counts_login_waits_per_site_without_a_usable_jar() -> Result<()> {
    let app = App::new(None)?;
    app.upload("x.com", &jar_text(VALUE, 4_102_444_800)).await?;
    let past = chrono::Utc::now() - chrono::Duration::days(30);
    let expired = format!(".youtube.com\tTRUE\t/\tTRUE\t{}\tSID\t{OTHER_VALUE}\n", (past + chrono::Duration::days(1)).timestamp());
    let jar = omni_core::cookies::parse_netscape_at("youtube.com", &expired, past)?;
    app.repo.put_cookie_jar("youtube.com", &jar, "admin", past)?;

    // Tidied off the review tab: left out.
    let tidied = job_in_review(&app.repo, "https://www.tiktok.com/@a/video/2", "1_ANNA_TEST", "LOGIN_REQUIRED")?;
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(app.repo.hide_old_review(0)?, 1);
    assert!(app.repo.get_job(tidied)?.unwrap().cleared_at.is_some());

    job_in_review(&app.repo, "https://www.instagram.com/p/abc", "2_ANNA_TEST", "LOGIN_REQUIRED")?;
    let insta_new = job_in_review(&app.repo, "https://www.instagram.com/reel/def", "3_ANNA_TEST", "LOGIN_REQUIRED")?;
    let yt = job_in_review(&app.repo, "https://www.youtube.com/watch?v=abc123", "4_ANNA_TEST", "LOGIN_REQUIRED")?;
    job_in_review(&app.repo, "https://news.example/story/1", "5_ANNA_TEST", "LOGIN_REQUIRED")?; // the player's wall, site unknown
    job_in_review(&app.repo, "https://www.tiktok.com/@a/video/1", "6_ANNA_TEST", "NO_STREAM_FOUND")?; // not a login wall
    let x = job_in_review(&app.repo, "https://x.com/someone/status/1?s=20", "7_ANNA_TEST", "LOGIN_REQUIRED")?;

    let (status, body) = app.send("GET", "/api/cookies/waiting", None, false).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body)?;
    assert_eq!(v["total"], 3, "{v}");
    assert_eq!(v["sites"][0]["site"], "instagram.com");
    assert_eq!((v["sites"][0]["count"].as_i64(), v["sites"][0]["jar"].as_str()), (Some(2), Some("missing")));
    assert_eq!((v["sites"][0]["newest_job_id"].as_i64(), v["sites"][0]["newest_slug"].as_str()), (Some(insta_new), Some("3_ANNA_TEST")));
    assert_eq!((v["sites"][1]["site"].as_str(), v["sites"][1]["jar"].as_str(), v["sites"][1]["newest_job_id"].as_i64()), (Some("youtube.com"), Some("expired"), Some(yt)));
    assert!(v["sites"].as_array().unwrap().iter().all(|s| s["site"] != "x.com"), "a usable jar is not waited for: {v}");
    assert!(!body.contains("https://") && !body.contains(VALUE) && !body.contains(OTHER_VALUE), "{body}");

    // The x.com jar stops logging in: its jobs now wait for it.
    app.repo.record_cookie_test("x.com", false, "login required", "https://x.com/a/status/1", chrono::Utc::now())?;
    let (_, body) = app.send("GET", "/api/cookies/waiting", None, false).await?;
    let v: Value = serde_json::from_str(&body)?;
    let xs = v["sites"].as_array().unwrap().iter().find(|s| s["site"] == "x.com").cloned().unwrap_or_default();
    assert_eq!((xs["count"].as_i64(), xs["jar"].as_str(), xs["newest_job_id"].as_i64()), (Some(1), Some("failed"), Some(x)), "{v}");
    Ok(())
}
