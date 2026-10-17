//! The admin's Benchmarks card routes (plan P6.4): what a start request is checked
//! for, how a refusal is reported, and that no response carries a corpus address.
//! The runner is the daemon's; here it is a fake. Who may call is in
//! `auth_matrix_tests.rs`. No network.

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

use omni_core::benchmark::Mode;
use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::{AppState, BenchRequest, BenchStartError, BenchmarkStarter};

const PEER: &str = "127.0.0.1:50000";

struct FakeStarter {
    answer: Mutex<Result<i64, BenchStartError>>,
    asked: Mutex<Vec<(Mode, Vec<i64>, u32, bool)>>,
}

impl BenchmarkStarter for FakeStarter {
    fn start(&self, request: BenchRequest) -> Result<i64, BenchStartError> {
        self.asked.lock().unwrap().push((request.mode, request.targets.clone(), request.parallel, request.force));
        (request.notify)();
        self.answer.lock().unwrap().clone()
    }
}

fn starter(answer: Result<i64, BenchStartError>) -> Arc<FakeStarter> {
    Arc::new(FakeStarter { answer: Mutex::new(answer), asked: Mutex::new(Vec::new()) })
}

struct App {
    router: axum::Router,
    repo: Repository,
    events: tokio::sync::broadcast::Receiver<String>,
    admin: String,
    _dir: TempDir,
}

impl App {
    fn new(starter: Option<Arc<FakeStarter>>) -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let id = repo.create_user("it@station.gr", "correct-horse-battery", UserRole::Admin, "Administrator", None)?;
        let admin = omni_core::auth::generate_session_token();
        repo.create_session(id, &admin, 1)?;
        let mut state = AppState::new(repo.clone(), AppConfig::default(), dir.path().join("config.json"));
        if let Some(s) = starter {
            state = state.with_benchmark_starter(s);
        }
        let events = state.events.subscribe();
        Ok(Self { router: WebServer::build_router(state), repo, events, admin, _dir: dir })
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Result<(StatusCode, String)> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", format!("omni_session={}", self.admin))
            .header("host", "mcr.local")
            .header("origin", "http://mcr.local")
            .header("x-omni-request", "1");
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
}

#[tokio::test]
async fn the_card_lists_the_corpus_without_any_address() -> Result<()> {
    let app = App::new(Some(starter(Ok(1))))?;
    let (status, body) = app.send("GET", "/api/admin/benchmarks", None).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let j: Value = serde_json::from_str(&body)?;
    assert_eq!(j["targets"].as_array().unwrap().len(), 65);
    assert_eq!(j["can_start"], true);
    assert_eq!(j["max_parallel"], 4);
    assert_eq!(j["targets"][60]["expected"], "expected_fail");
    assert!(!body.contains("http://") && !body.contains("https://"), "the list shows an address");
    assert!(app.repo.benchmark_targets()?.len() == 65, "the first look stores the corpus");
    Ok(())
}

#[tokio::test]
async fn a_start_request_is_checked_and_handed_to_the_runner() -> Result<()> {
    let s = starter(Ok(7));
    let mut app = App::new(Some(s.clone()))?;
    let (status, body) = app.send("POST", "/api/admin/benchmarks/run", Some(json!({"mode": "extract", "targets": "1-5,9", "parallel": 3}))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body)?["run_id"], 7);
    assert_eq!(s.asked.lock().unwrap().as_slice(), &[(Mode::Extract, vec![1, 2, 3, 4, 5, 9], 3, false)]);
    // The card refetches on any event: the start announced itself.
    assert_eq!(app.events.try_recv().as_deref(), Ok(r#"{"type":"benchmark"}"#));

    // An empty target list means the whole corpus; the default is two at once; force is passed on.
    let (status, _) = app.send("POST", "/api/admin/benchmarks/run", Some(json!({"mode": "full", "force": true}))).await?;
    assert_eq!(status, StatusCode::OK);
    let asked = s.asked.lock().unwrap().clone();
    assert_eq!((asked[1].0, asked[1].1.len(), asked[1].2, asked[1].3), (Mode::Full, 65, 2, true));
    let audit = app.repo.get_recent_audit_logs(10)?.into_iter().map(|a| a.message).collect::<Vec<_>>().join("\n");
    assert!(audit.contains("started benchmark run 7"), "{audit}");
    Ok(())
}

#[tokio::test]
async fn bad_requests_never_reach_the_runner() -> Result<()> {
    let s = starter(Ok(1));
    let app = App::new(Some(s.clone()))?;
    for body in [
        json!({"mode": "deliver", "targets": "1-5"}),
        json!({"mode": "extract", "targets": "0-5"}),
        json!({"mode": "extract", "targets": "1-66"}),
        json!({"mode": "extract", "targets": "five"}),
        json!({"mode": "extract", "targets": "1-5", "parallel": 0}),
        json!({"mode": "extract", "targets": "1-5", "parallel": 5}),
    ] {
        let (status, text) = app.send("POST", "/api/admin/benchmarks/run", Some(body.clone())).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
    }
    assert!(s.asked.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn a_refusal_is_a_409_with_its_reason_and_no_runner_is_a_409_too() -> Result<()> {
    let app = App::new(Some(starter(Err(BenchStartError::Refused("2 job(s) are running; tick force to run anyway.".into())))))?;
    let (status, body) = app.send("POST", "/api/admin/benchmarks/run", Some(json!({"mode": "extract"}))).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("2 job(s) are running"), "{body}");

    let app = App::new(None)?;
    let (status, _) = app.send("POST", "/api/admin/benchmarks/run", Some(json!({"mode": "extract"}))).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, list) = app.send("GET", "/api/admin/benchmarks", None).await?;
    assert_eq!(serde_json::from_str::<Value>(&list)?["can_start"], false);
    Ok(())
}

#[tokio::test]
async fn a_report_is_served_by_run_number() -> Result<()> {
    let app = App::new(None)?;
    let (status, _) = app.send("GET", "/api/admin/benchmarks/runs/99", None).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);

    app.repo.seed_benchmark_targets()?;
    let omni_core::repository::BenchStart::Started(id) =
        app.repo.begin_benchmark_run(Mode::Extract, "{}", chrono::Utc::now(), chrono::Duration::hours(6), "cli")?
    else {
        panic!()
    };
    let (status, body) = app.send("GET", &format!("/api/admin/benchmarks/runs/{id}"), None).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let j: Value = serde_json::from_str(&body)?;
    assert_eq!(j["run"]["id"], id);
    assert_eq!(j["run"]["state"], "running");
    assert!(j["run"]["finished_at"].is_null(), "an unfinished run has no end time, not the current time");
    Ok(())
}
