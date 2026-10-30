//! `GET /metrics` (plan P6.5): off by default, and when on, who may scrape and
//! what the body may contain.
//!
//! Each test builds its own recorder with `build_recorder()` and never installs
//! a global one, so tests in this binary cannot fight over it.

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::health::Check;
use omni_core::models::{JobStage, JobStatus, UserRole};
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::{AppState, MetricsSources};

/// Not in `metrics.allow_networks` (loopback by default) and not on the MCR list.
const OUTSIDER: &str = "192.0.2.77:51000";
/// On `security.mcr_open_networks` in these tests, but not on `metrics.allow_networks`.
const MCR_ROOM: &str = "10.20.30.40:51000";
const LOOPBACK: &str = "127.0.0.1:51000";

const SECRET_URL: &str = "https://leak.example/secret-clip-xyz";
const SECRET_SLUG: &str = "1_ZZQUOKKA_SECRETSLUG";

struct Harness {
    router: axum::Router,
    admin: String,
    mcr: String,
    user: String,
    dir: TempDir,
    state: AppState,
}

fn harness(configure: impl FnOnce(&mut AppConfig), with_recorder: bool) -> Harness {
    let dir = TempDir::new().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    let mut config = AppConfig::default();
    config.security.mcr_open_networks = vec!["10.20.0.0/16".to_string()];
    configure(&mut config);

    let token = |email: &str, role: UserRole| {
        let id = repo.create_user(email, "correct-horse-battery", role, "Someone", None).unwrap();
        let t = omni_core::auth::generate_session_token();
        repo.create_session(id, &t, 1).unwrap();
        t
    };
    let admin = token("it@station.test", UserRole::Admin);
    let mcr = token("desk@station.test", UserRole::OpenMcr);
    let user = token("reporter@station.test", UserRole::User);

    repo.add_job(SECRET_URL, SECRET_SLUG, "ZZQUOKKA", "SECRET", "1", 0, JobStatus::Pending, None, None, None)
        .unwrap();

    let mut state = AppState::new(repo.clone(), config, dir.path().join("config.json"));
    if with_recorder {
        let handle = PrometheusBuilder::new().build_recorder().handle();
        state = state.with_metrics(
            handle,
            MetricsSources {
                temp: dir.path().to_path_buf(),
                watchfolder: dir.path().to_path_buf(),
                archive: Some(dir.path().to_path_buf()),
                workers_max: 4,
            },
        );
    }
    Harness { router: WebServer::build_router(state.clone()), admin, mcr, user, dir, state }
}

fn on(config: &mut AppConfig) {
    config.metrics.enabled = true;
}

async fn get(h: &Harness, peer: &str, token: Option<&str>) -> (StatusCode, String, String) {
    get_with(h, peer, token, None).await
}

/// `(status, content-type, body)`.
async fn get_with(h: &Harness, peer: &str, token: Option<&str>, xff: Option<&str>) -> (StatusCode, String, String) {
    let peer: SocketAddr = peer.parse().unwrap();
    let mut req = Request::builder().method("GET").uri("/metrics");
    if let Some(t) = token {
        req = req.header("cookie", format!("omni_session={t}"));
    }
    if let Some(x) = xff {
        req = req.header("x-forwarded-for", x);
    }
    let resp = h.router.clone().layer(MockConnectInfo(peer)).oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let ctype = resp.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, ctype, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn off_by_default_it_is_a_404_for_everyone() {
    // Default config, no recorder: the shipped state.
    let h = harness(|_| {}, false);
    for (peer, token) in [
        (LOOPBACK, None),
        (OUTSIDER, None),
        (MCR_ROOM, None),
        (LOOPBACK, Some(h.admin.as_str())),
        (OUTSIDER, Some(h.admin.as_str())),
    ] {
        let (status, _, body) = get(&h, peer, token).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{peer} {token:?}");
        assert!(body.is_empty(), "the body of an unknown route is empty: {body}");
    }

    // A recorder present but the switch off (config reloaded, say): still nothing.
    let h = harness(|_| {}, true);
    assert_eq!(get(&h, LOOPBACK, Some(&h.admin)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_allowlisted_client_gets_every_status_in_the_text_format() {
    let h = harness(on, true);
    let (status, ctype, body) = get(&h, LOOPBACK, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/plain"), "{ctype}");
    assert!(ctype.contains("version=0.0.4"), "{ctype}");
    assert!(body.contains("omni_jobs{status=\"PENDING\"} 1"), "{body}");
    for s in JobStatus::ALL {
        assert!(body.contains(&format!("omni_jobs{{status=\"{}\"}} ", s.as_str())), "{} missing: {body}", s.as_str());
    }
    assert!(body.contains("omni_jobs{status=\"COMPLETED\"} 0"), "{body}");
    assert!(body.contains("omni_workers_busy 0"), "{body}");
    assert!(body.contains("omni_workers_max 4"), "{body}");
    for volume in ["temp", "watchfolder", "archive"] {
        assert!(body.contains(&format!("omni_disk_free_bytes{{volume=\"{volume}\"}} ")), "{volume}: {body}");
    }
}

#[tokio::test]
async fn a_client_outside_the_list_is_refused() {
    let h = harness(on, true);
    assert_eq!(get(&h, OUTSIDER, None).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_admin_session_may_scrape_from_anywhere() {
    let h = harness(on, true);
    let (status, _, body) = get(&h, OUTSIDER, Some(&h.admin)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("omni_jobs{status=\"PENDING\"}"), "{body}");
}

#[tokio::test]
async fn mcr_access_and_other_sessions_do_not_grant_a_scrape() {
    let h = harness(on, true);
    // The MCR allowlist alone, an MCR session and a user session, each from outside the metrics list.
    assert_eq!(get(&h, MCR_ROOM, None).await.0, StatusCode::FORBIDDEN, "mcr_open_networks");
    assert_eq!(get(&h, OUTSIDER, Some(&h.mcr)).await.0, StatusCode::FORBIDDEN, "MCR session");
    assert_eq!(get(&h, OUTSIDER, Some(&h.user)).await.0, StatusCode::FORBIDDEN, "user session");
    assert_eq!(get(&h, MCR_ROOM, Some(&h.user)).await.0, StatusCode::FORBIDDEN, "user session, MCR room");
}

#[tokio::test]
async fn the_allow_list_is_configurable_and_a_bad_entry_is_skipped() {
    let h = harness(
        |c| {
            c.metrics.enabled = true;
            c.metrics.allow_networks = vec!["not-a-network".into(), "192.0.2.0/24".into()];
        },
        true,
    );
    assert_eq!(get(&h, OUTSIDER, None).await.0, StatusCode::OK);
    // A set list replaces the loopback default.
    assert_eq!(get(&h, LOOPBACK, None).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_forwarded_client_is_judged_by_the_same_proxy_rules_as_the_rest() {
    let h = harness(
        |c| {
            c.metrics.enabled = true;
            c.security.trust_proxy_header = true;
            c.security.trusted_proxies = vec!["127.0.0.1/32".into()];
        },
        true,
    );
    // Through the trusted local proxy, the original client counts, not the proxy.
    assert_eq!(get_with(&h, LOOPBACK, None, Some("192.0.2.5")).await.0, StatusCode::FORBIDDEN);
    assert_eq!(get_with(&h, LOOPBACK, None, Some("127.0.0.1")).await.0, StatusCode::OK);
    // An untrusted peer's header is ignored.
    let h = harness(on, true);
    assert_eq!(get_with(&h, OUTSIDER, None, Some("127.0.0.1")).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_body_carries_no_url_slug_name_address_or_path() {
    let h = harness(on, true);
    // A health detail with a path and an address in it must not reach the scrape.
    h.state.health.set(
        "tools",
        Check::degraded("ffmpeg.exe not found in D:\\OmniDownloader\\bin for ingest@station.test"),
    );
    h.state.health.set("mail", Check::down("authentication failed for ingest@station.test").with_error("secret-token-abc"));
    h.state.health.note_mail_poll();
    let (status, _, body) = get(&h, LOOPBACK, None).await;
    assert_eq!(status, StatusCode::OK);
    for needle in [
        SECRET_URL,
        "secret-clip-xyz",
        SECRET_SLUG,
        "ZZQUOKKA",
        "station.test",
        "secret-token-abc",
        "OmniDownloader",
        "ffmpeg.exe",
        h.dir.path().to_str().unwrap(),
    ] {
        assert!(!body.contains(needle), "{needle} leaked: {body}");
    }
    // Health and the mail poll are there, as numbers.
    assert!(body.contains("omni_health{check=\"tools\"} 1"), "{body}");
    assert!(body.contains("omni_health{check=\"mail\"} 2"), "{body}");
    assert!(body.contains("omni_mail_last_poll_age_seconds "), "{body}");
}

#[tokio::test]
async fn the_mail_poll_age_is_left_out_until_a_poll_has_succeeded() {
    let h = harness(on, true);
    let (_, _, body) = get(&h, LOOPBACK, None).await;
    assert!(!body.contains("omni_mail_last_poll_age_seconds"), "{body}");
    h.state.health.note_mail_poll();
    let (_, _, body) = get(&h, LOOPBACK, None).await;
    let line = body.lines().find(|l| l.starts_with("omni_mail_last_poll_age_seconds ")).expect("age line");
    let age: f64 = line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!((0.0..60.0).contains(&age), "{line}");
}

#[tokio::test]
async fn busy_workers_are_counted_at_scrape_time() {
    let h = harness(on, true);
    let _a = h.state.busy_jobs.guard(1, "w1");
    let _b = h.state.busy_jobs.guard(2, "w2");
    let (_, _, body) = get(&h, LOOPBACK, None).await;
    assert!(body.contains("omni_workers_busy 2"), "{body}");
}

#[tokio::test]
async fn the_scrape_includes_what_the_recorder_holds_for_stages_and_finished_jobs() {
    let recorder = PrometheusBuilder::new().build_recorder();
    let dir = TempDir::new().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    let mut config = AppConfig::default();
    config.metrics.enabled = true;
    let state = AppState::new(repo.clone(), config, dir.path().join("config.json")).with_metrics(
        recorder.handle(),
        MetricsSources { temp: dir.path().into(), watchfolder: dir.path().into(), archive: None, workers_max: 1 },
    );
    let router = WebServer::build_router(state);

    let id = repo.add_job(SECRET_URL, SECRET_SLUG, "ZZQUOKKA", "SECRET", "1", 0, JobStatus::Pending, None, None, None).unwrap();
    let owner = "test-host:1:0";
    repo.lease_job(owner, 120).unwrap().expect("leased");
    metrics::with_local_recorder(&recorder, || {
        assert!(repo.set_stage(id, owner, JobStage::Download).unwrap());
        assert!(repo.set_stage(id, owner, JobStage::Probe).unwrap());
        assert!(repo.finish(id, owner, JobStatus::Failed, Some("X"), None, None).unwrap());
    });

    let req = Request::builder().method("GET").uri("/metrics").body(Body::empty()).unwrap();
    let peer: SocketAddr = LOOPBACK.parse().unwrap();
    let resp = router.layer(MockConnectInfo(peer)).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(body.contains("omni_stage_duration_seconds"), "{body}");
    assert!(body.contains("stage=\"download\""), "{body}");
    assert!(body.contains("stage=\"probe\""), "{body}");
    assert!(body.contains("omni_jobs_finished_total{status=\"FAILED\"} 1"), "{body}");
    assert!(!body.contains(SECRET_SLUG) && !body.contains(SECRET_URL), "{body}");
}

