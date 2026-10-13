//! The admin's «Test URL» routes (plan P10.15b): what they refuse before the
//! dry run, and what they pass on. The dry run itself is the daemon's and is
//! tested there; here it is a fake. Who may call is in `auth_matrix_tests.rs`.
//! No network; invented addresses.

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
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::{AppState, TestUrlError, TestUrlFuture, UrlTester};

const PEER: &str = "127.0.0.1:50000";

/// Answers as told and remembers what it was asked.
struct FakeTester {
    answer: Mutex<Result<Value, TestUrlError>>,
    asked: Mutex<Vec<(String, bool)>>,
}

impl UrlTester for FakeTester {
    fn test_url<'a>(&'a self, url: &'a str, use_llm: bool) -> TestUrlFuture<'a> {
        Box::pin(async move {
            self.asked.lock().unwrap().push((url.to_string(), use_llm));
            self.answer.lock().unwrap().clone()
        })
    }
}

fn tester(answer: Result<Value, TestUrlError>) -> Arc<FakeTester> {
    Arc::new(FakeTester { answer: Mutex::new(answer), asked: Mutex::new(Vec::new()) })
}

struct App {
    router: axum::Router,
    repo: Repository,
    admin: String,
    _dir: TempDir,
}

impl App {
    fn new(tester: Option<Arc<FakeTester>>, config: AppConfig) -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let id = repo.create_user("it@station.gr", "correct-horse-battery", UserRole::Admin, "Administrator", None)?;
        let admin = omni_core::auth::generate_session_token();
        repo.create_session(id, &admin, 1)?;
        let mut state = AppState::new(repo.clone(), config, dir.path().join("config.json"));
        if let Some(t) = tester {
            state = state.with_url_tester(t);
        }
        Ok(Self { router: WebServer::build_router(state), repo, admin, _dir: dir })
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>, marker: bool) -> Result<(StatusCode, Value)> {
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
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }

    async fn run(&self, url: &str, use_llm: bool) -> Result<(StatusCode, Value)> {
        self.send("POST", "/api/admin/test-url", Some(json!({ "url": url, "use_llm": use_llm })), true).await
    }
}

#[tokio::test]
async fn a_private_or_odd_address_is_refused_before_the_test_is_asked() -> Result<()> {
    let t = tester(Ok(json!({})));
    let app = App::new(Some(t.clone()), AppConfig::default())?;
    for url in [
        "http://127.0.0.1/a",
        "http://192.168.1.20/a",
        "http://localhost/a",
        "http://[::1]/a",
        "ftp://news.example/a",
        "file:///c:/windows/win.ini",
        "https://user:pass@news.example/a",
        "not a url",
        "",
    ] {
        let (status, _) = app.run(url, false).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url}");
    }
    let (status, _) = app.run(&format!("https://news.example/{}", "a".repeat(2100)), false).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "too long");
    assert!(t.asked.lock().unwrap().is_empty(), "the dry run never saw any of them");
    Ok(())
}

#[tokio::test]
async fn a_public_address_runs_the_test_and_passes_the_report_and_the_tick_on() -> Result<()> {
    let t = tester(Ok(json!({ "verdict": "ok", "total_ms": 5 })));
    let app = App::new(Some(t.clone()), AppConfig::default())?;
    let (status, json) = app.run("https://news.example/story/secret-path?token=abc", true).await?;
    assert_eq!((status, json["verdict"].as_str()), (StatusCode::OK, Some("ok")));
    app.run("https://news.example/story/2", false).await?;
    let asked = t.asked.lock().unwrap().clone();
    assert_eq!(asked[0], ("https://news.example/story/secret-path?token=abc".to_string(), true));
    assert_eq!(asked[1].1, false, "unticked stays unticked");
    let audit = app.repo.get_recent_audit_logs(20)?.into_iter().map(|a| a.message).collect::<Vec<_>>().join("\n");
    assert!(audit.contains("Test URL on news.example"), "{audit}");
    assert!(!audit.contains("secret-path") && !audit.contains("token"), "the audit line names the host only: {audit}");
    Ok(())
}

#[tokio::test]
async fn a_second_test_gets_409_and_a_refusal_gets_400() -> Result<()> {
    let t = tester(Err(TestUrlError::Busy));
    let app = App::new(Some(t.clone()), AppConfig::default())?;
    let (status, json) = app.run("https://news.example/a", false).await?;
    assert_eq!((status, json["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("BUSY")));
    *t.answer.lock().unwrap() = Err(TestUrlError::Refused("host x does not resolve to a public address".into()));
    let (status, json) = app.run("https://news.example/a", false).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["error"]["message"].as_str().unwrap_or("").contains("public address"), "{json}");
    Ok(())
}

#[tokio::test]
async fn without_a_tester_the_test_is_unavailable_and_without_the_panel_marker_it_is_forbidden() -> Result<()> {
    let app = App::new(None, AppConfig::default())?;
    let (status, json) = app.run("https://news.example/a", false).await?;
    assert_eq!((status, json["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("UNAVAILABLE")));
    let (_, info) = app.send("GET", "/api/admin/test-url", None, true).await?;
    assert_eq!(info["can_run"], false);

    let t = tester(Ok(json!({})));
    let app = App::new(Some(t.clone()), AppConfig::default())?;
    let (status, _) = app.send("POST", "/api/admin/test-url", Some(json!({ "url": "https://news.example/a" })), false).await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "CSRF: the panel marker is required");
    assert!(t.asked.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn the_card_learns_whether_the_model_is_usable_and_how_many_calls_are_left_today() -> Result<()> {
    let mut config = AppConfig::default();
    let app = App::new(Some(tester(Ok(json!({})))), config.clone())?;
    let (_, info) = app.send("GET", "/api/admin/test-url", None, true).await?;
    assert_eq!((info["can_run"].clone(), info["llm_available"].clone()), (json!(true), json!(false)), "off by default");

    config.repair.llm_enabled = true;
    config.repair.llm_model = Some("model-x".into());
    config.repair.llm_max_calls_per_day = 5;
    let app = App::new(Some(tester(Ok(json!({})))), config)?;
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert!(app.repo.reserve_llm_call(&day, 5)?);
    assert!(app.repo.reserve_llm_call(&day, 5)?);
    let (_, info) = app.send("GET", "/api/admin/test-url", None, true).await?;
    assert_eq!((info["llm_available"].clone(), info["llm_cap"].clone(), info["llm_left"].clone()), (json!(true), json!(5), json!(3)));
    Ok(())
}
