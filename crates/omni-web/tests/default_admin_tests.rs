//! The first-run `admin` / `Admin` account (P2.14): it signs in only from the
//! server itself, and while its password is the default its session can only
//! change that password, create a user or sign out.

use std::net::SocketAddr;

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::{AppState, MetricsSources};

const NORMAL: &str = "correct-horse-battery";
const REMOTE: &str = "10.0.0.9:51000";
const LOOPBACK: &str = "127.0.0.1:51000";

struct App {
    router: axum::Router,
    repo: Repository,
    state: AppState,
    dir: TempDir,
}

struct Reply {
    status: StatusCode,
    cookie: Option<String>,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap_or("").to_string()
    }
}

/// A database with the first-run account and one ordinary administrator.
fn app(configure: impl FnOnce(&mut AppConfig), metrics: bool) -> Result<App> {
    app_with(configure, metrics, false)
}

/// `default_inactive`: the default account is deactivated when the state is built,
/// so the cached flag starts false.
fn app_with(configure: impl FnOnce(&mut AppConfig), metrics: bool, default_inactive: bool) -> Result<App> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    repo.create_user("admin", "Admin", UserRole::Admin, "First-run administrator", None)?;
    repo.create_user("it@newsroom.local", NORMAL, UserRole::Admin, "Administrator", None)?;
    if default_inactive {
        let id = repo.get_user_by_email("admin")?.unwrap().id;
        repo.set_user_active_status(id, false)?;
    }
    let mut config = AppConfig::default();
    config.security.login_rate_limit_per_5min = 10_000;
    config.security.login_rate_limit_per_account_hour = 10_000;
    configure(&mut config);
    config.save_to_file(&dir.path().join("config.json"))?;
    let mut state = AppState::new(repo.clone(), config, dir.path().join("config.json"))
        .with_secret_store(omni_core::secrets::SecretStore::new(dir.path().join("secrets.bin")));
    if metrics {
        state = state.with_metrics(
            PrometheusBuilder::new().build_recorder().handle(),
            MetricsSources {
                temp: dir.path().to_path_buf(),
                watchfolder: dir.path().to_path_buf(),
                archive: None,
                workers_max: 1,
            },
        );
    }
    Ok(App { router: WebServer::build_router(state.clone()), repo, state, dir })
}

async fn send(
    app: &App,
    method: &str,
    uri: &str,
    peer: &str,
    token: Option<&str>,
    xff: Option<&str>,
    body: Option<Value>,
) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("cookie", format!("omni_session={t}"));
    }
    if let Some(x) = xff {
        req = req.header("x-forwarded-for", x);
    }
    let body = match body {
        Some(b) => {
            req = req
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .header("host", "mcr.local")
                .header("origin", "http://mcr.local");
            Body::from(b.to_string())
        }
        None if method != "GET" => {
            req = req
                .header("x-omni-request", "1")
                .header("host", "mcr.local")
                .header("origin", "http://mcr.local");
            Body::empty()
        }
        None => Body::empty(),
    };
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo(peer.parse::<SocketAddr>().unwrap()))
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let cookie = res.headers().get("set-cookie").and_then(|v| v.to_str().ok()).map(str::to_string);
    let body = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    Reply { status, cookie, body }
}

async fn login(app: &App, peer: &str, xff: Option<&str>, email: &str, password: &str) -> Reply {
    send(app, "POST", "/api/auth/login", peer, None, xff, Some(json!({"email": email, "password": password}))).await
}

fn token_of(reply: &Reply) -> String {
    reply
        .cookie
        .as_deref()
        .expect("a session cookie")
        .split(';')
        .next()
        .unwrap()
        .trim()
        .strip_prefix("omni_session=")
        .unwrap()
        .to_string()
}

fn session_count(app: &App) -> i64 {
    let conn = rusqlite::Connection::open(app.dir.path().join("omni.db")).unwrap();
    conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)).unwrap()
}

/// The default account signed in from the server itself.
async fn default_session(app: &App) -> String {
    let r = login(app, LOOPBACK, None, "admin", "Admin").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    token_of(&r)
}

async fn normal_session(app: &App) -> String {
    let r = login(app, REMOTE, None, "it@newsroom.local", NORMAL).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    token_of(&r)
}

// ---------------------------------------------------------------- (A)

#[tokio::test]
async fn the_default_login_from_another_host_is_refused_like_a_wrong_password() -> Result<()> {
    let a = app(|_| {}, false)?;

    let refused = login(&a, REMOTE, None, "admin", "Admin").await;
    let wrong = login(&a, "10.0.0.10:1", None, "admin", "not-the-password").await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.status, wrong.status);
    assert_eq!(refused.body, wrong.body, "the body must not tell the caller why");
    assert!(refused.cookie.is_none(), "no session cookie");
    assert_eq!(session_count(&a), 0, "no session row");

    let attempts = a.repo.recent_login_attempts(10)?;
    assert!(
        attempts.iter().any(|t| t.reason.as_deref() == Some("default_admin_remote") && !t.successful),
        "{attempts:?}"
    );
    let audit = a.repo.get_recent_audit_logs(10)?;
    let row = audit
        .iter()
        .find(|l| l.category == "AUTH" && l.message.contains("10.0.0.9"))
        .expect("an audit row naming the client");
    assert_eq!(row.level, "WARN");
    assert!(!row.message.contains("Admin /") && !row.message.to_lowercase().contains("password:"), "{}", row.message);

    // From the server itself it works, and says the password must change.
    let local = login(&a, LOOPBACK, None, "Admin", "Admin").await;
    assert_eq!(local.status, StatusCode::OK);
    assert_eq!(local.json()["password_change_required"], json!(true));
    assert_eq!(session_count(&a), 1);
    Ok(())
}

#[tokio::test]
async fn a_normal_administrator_signs_in_from_another_host() -> Result<()> {
    // Negative control: the rule is about the default login only.
    let a = app(|_| {}, false)?;
    let r = login(&a, REMOTE, None, "it@newsroom.local", NORMAL).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["password_change_required"], json!(false));
    Ok(())
}

#[tokio::test]
async fn a_trusted_proxy_does_not_hide_a_remote_default_login() -> Result<()> {
    let a = app(
        |c| {
            c.security.trust_proxy_header = true;
            c.security.trusted_proxies = vec!["127.0.0.1/32".to_string()];
        },
        false,
    )?;
    // The peer is the proxy on this machine; the client it forwards is not.
    let r = login(&a, LOOPBACK, Some("10.0.0.9"), "admin", "Admin").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(session_count(&a), 0);
    // A forwarded loopback client is the server itself.
    let r = login(&a, LOOPBACK, Some("127.0.0.1"), "admin", "Admin").await;
    assert_eq!(r.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn the_rule_guards_the_default_password_only() -> Result<()> {
    let a = app(|_| {}, false)?;
    let token = default_session(&a).await;
    let r = send(
        &a,
        "POST",
        "/api/auth/password",
        LOOPBACK,
        Some(&token),
        None,
        Some(json!({"current_password": "Admin", "new_password": "a-brand-new-passphrase"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    let r = login(&a, REMOTE, None, "admin", "a-brand-new-passphrase").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["password_change_required"], json!(false));
    Ok(())
}

// ---------------------------------------------------------------- (B)

#[tokio::test]
async fn a_default_session_is_limited_to_the_change_form_and_a_normal_admin_is_not() -> Result<()> {
    let a = app(|_| {}, false)?;
    let locked = default_session(&a).await;
    let normal = normal_session(&a).await;

    // Refused: MCR, user and admin routes alike.
    for uri in ["/api/jobs", "/api/jobs/mine", "/api/admin/selfcheck", "/api/user/prefs", "/api/admin/tools"] {
        let r = send(&a, "GET", uri, LOOPBACK, Some(&locked), None, None).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(r.code(), "PASSWORD_CHANGE_REQUIRED", "{uri}");
    }
    // Allowed: who am I, the user list, creating a user, signing out.
    let me = send(&a, "GET", "/api/auth/me", LOOPBACK, Some(&locked), None, None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["password_change_required"], json!(true));
    assert_eq!(send(&a, "GET", "/api/admin/users", LOOPBACK, Some(&locked), None, None).await.status, StatusCode::OK);
    let made = send(
        &a,
        "POST",
        "/api/admin/users",
        LOOPBACK,
        Some(&locked),
        None,
        Some(json!({"email": "chief@newsroom.local", "full_name": "Chief", "password": NORMAL, "role": "admin"})),
    )
    .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert_eq!(send(&a, "GET", "/admin", LOOPBACK, Some(&locked), None, None).await.status, StatusCode::OK);
    assert_eq!(
        send(&a, "GET", "/mcr", LOOPBACK, Some(&locked), None, None).await.status,
        StatusCode::TEMPORARY_REDIRECT
    );

    // Negative control: the other administrator is unaffected while the default account is live.
    for uri in ["/api/jobs", "/api/admin/selfcheck", "/api/admin/tools", "/api/auth/me"] {
        let r = send(&a, "GET", uri, REMOTE, Some(&normal), None, None).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.body);
    }
    assert_eq!(send(&a, "GET", "/api/auth/me", REMOTE, Some(&normal), None, None).await.json()["password_change_required"], json!(false));

    let out = send(&a, "POST", "/api/auth/logout", LOOPBACK, Some(&locked), None, None).await;
    assert_eq!(out.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn a_default_session_can_sign_out_everywhere() -> Result<()> {
    let a = app(|_| {}, false)?;
    let locked = default_session(&a).await;
    let _second = default_session(&a).await;
    let out = send(&a, "POST", "/api/auth/logout-all", LOOPBACK, Some(&locked), None, None).await;
    assert_eq!(out.status, StatusCode::OK, "{}", out.body);
    assert_eq!(session_count(&a), 0, "every session of the default account ends");
    Ok(())
}

#[tokio::test]
async fn setup_and_metrics_do_not_treat_the_default_session_as_an_admin() -> Result<()> {
    let a = app(|c| c.metrics.enabled = true, true)?;
    let locked = default_session(&a).await;
    let normal = normal_session(&a).await;
    let outside = "192.0.2.77:51000";

    // /setup is closed to it (404, as when closed) and open to a real admin.
    assert_eq!(send(&a, "GET", "/setup", LOOPBACK, Some(&locked), None, None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(send(&a, "GET", "/setup", REMOTE, Some(&normal), None, None).await.status, StatusCode::OK);
    // /metrics: refused for it, served to a real admin.
    assert_eq!(send(&a, "GET", "/metrics", outside, Some(&locked), None, None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&a, "GET", "/metrics", outside, Some(&normal), None, None).await.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn changing_the_password_lifts_the_limit_without_a_restart() -> Result<()> {
    let a = app(|_| {}, false)?;
    let locked = default_session(&a).await;
    assert!(a.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    let r = send(
        &a,
        "POST",
        "/api/auth/password",
        LOOPBACK,
        Some(&locked),
        None,
        Some(json!({"current_password": "Admin", "new_password": "a-brand-new-passphrase"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!a.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));

    let r = login(&a, LOOPBACK, None, "admin", "a-brand-new-passphrase").await;
    let fresh = token_of(&r);
    assert_eq!(send(&a, "GET", "/api/jobs", LOOPBACK, Some(&fresh), None, None).await.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn another_admin_resetting_the_default_password_lifts_the_limit() -> Result<()> {
    let a = app(|_| {}, false)?;
    let normal = normal_session(&a).await;
    let id = a.repo.get_user_by_email("admin")?.unwrap().id;
    let r = send(
        &a,
        "POST",
        &format!("/api/admin/users/{id}/password"),
        REMOTE,
        Some(&normal),
        None,
        Some(json!({"password": "a-brand-new-passphrase"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!a.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    let fresh = token_of(&login(&a, LOOPBACK, None, "admin", "a-brand-new-passphrase").await);
    assert_eq!(send(&a, "GET", "/api/jobs", LOOPBACK, Some(&fresh), None, None).await.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn deactivating_the_default_account_lifts_the_flag() -> Result<()> {
    let a = app(|_| {}, false)?;
    let normal = normal_session(&a).await;
    let id = a.repo.get_user_by_email("admin")?.unwrap().id;
    assert!(a.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    let r = send(
        &a,
        "POST",
        &format!("/api/admin/users/{id}/active"),
        REMOTE,
        Some(&normal),
        None,
        Some(json!({"active": false})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!a.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    Ok(())
}

// ---------------------------------------------------------------- (C)

fn setup_body(endpoint: &str) -> Value {
    json!({
        "graph_tenant_id": "", "graph_client_id": "", "graph_mailbox": "",
        "ollama_endpoint": endpoint, "ollama_model": "m", "watchfolder_path": "C:\\\\Playout\\\\Watch"
    })
}

#[tokio::test]
async fn setup_refuses_an_online_http_llm_endpoint() -> Result<()> {
    let a = app(|_| {}, false)?;
    let normal = normal_session(&a).await;
    let r = send(&a, "POST", "/api/setup", REMOTE, Some(&normal), None, Some(setup_body("http://evil.example/v1"))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert_ne!(a.state.config.read().await.ollama_endpoint, "http://evil.example/v1");
    Ok(())
}

#[tokio::test]
async fn setup_drops_the_llm_key_when_the_base_changes_and_keeps_it_when_not() -> Result<()> {
    let a = app(
        |c| {
            c.ollama_endpoint = "https://llm.newsroom.example/v1".to_string();
            c.llm.api_key = "sk-invented".to_string();
        },
        false,
    )?;
    let key = omni_core::secrets::keys::LLM_API_KEY;
    a.state.secrets.set(key, "sk-invented")?;
    let normal = normal_session(&a).await;

    // Same base (trailing slash and case aside): the key stays.
    let r = send(&a, "POST", "/api/setup", REMOTE, Some(&normal), None, Some(setup_body("https://LLM.newsroom.example/v1/"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(a.state.secrets.get(key)?.as_deref(), Some("sk-invented"));
    assert_eq!(a.state.config.read().await.llm.api_key, "sk-invented");

    // A new provider does not inherit it.
    let r = send(&a, "POST", "/api/setup", REMOTE, Some(&normal), None, Some(setup_body("https://other.newsroom.example/v1"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(a.state.secrets.get(key)?, None);
    assert_eq!(a.state.config.read().await.llm.api_key, "");
    Ok(())
}

// ---------------------------------------------------------------- stale flag, start-up, blank endpoint

#[tokio::test]
async fn a_stale_false_flag_is_corrected_by_the_default_login_itself() -> Result<()> {
    let a = app_with(|_| {}, false, true)?;
    assert!(!a.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    // The CLI reactivates the account behind the daemon's back.
    let id = a.repo.list_users()?.into_iter().find(|u| u.email == "admin").unwrap().id;
    a.repo.set_user_active_status(id, true)?;

    let r = login(&a, LOOPBACK, None, "admin", "Admin").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["password_change_required"], json!(true));
    let token = token_of(&r);
    let denied = send(&a, "GET", "/api/admin/selfcheck", LOOPBACK, Some(&token), None, None).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.code(), "PASSWORD_CHANGE_REQUIRED");
    Ok(())
}

#[tokio::test]
async fn a_stale_true_flag_is_corrected_when_the_password_was_changed_elsewhere() -> Result<()> {
    let a = app(|_| {}, false)?;
    assert!(a.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    let id = a.repo.get_user_by_email("admin")?.unwrap().id;
    a.repo.update_user_password(id, "a-brand-new-passphrase")?;
    let r = login(&a, LOOPBACK, None, "admin", "a-brand-new-passphrase").await;
    assert_eq!(r.json()["password_change_required"], json!(false));
    assert!(!a.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    Ok(())
}

#[tokio::test]
async fn start_up_ends_default_account_sessions_only_while_the_password_is_live() -> Result<()> {
    let a = app(|_| {}, false)?;
    let default_id = a.repo.get_user_by_email("admin")?.unwrap().id;
    let other_id = a.repo.get_user_by_email("it@newsroom.local")?.unwrap().id;
    a.repo.create_session(default_id, "old-network-session", 1)?;
    a.repo.create_session(other_id, "other-session", 1)?;

    assert_eq!(a.state.end_default_admin_sessions(), 1);
    assert!(a.repo.get_user_by_session_token("old-network-session")?.is_none());
    assert!(a.repo.get_user_by_session_token("other-session")?.is_some(), "other accounts keep theirs");

    // Password already changed: nothing to end.
    a.repo.update_user_password(default_id, "a-brand-new-passphrase")?;
    a.state.refresh_default_admin();
    a.repo.create_session(default_id, "kept-session", 1)?;
    assert_eq!(a.state.end_default_admin_sessions(), 0);
    assert!(a.repo.get_user_by_session_token("kept-session")?.is_some());
    Ok(())
}

#[tokio::test]
async fn setup_accepts_a_blank_llm_endpoint() -> Result<()> {
    let a = app(|_| {}, false)?;
    let normal = normal_session(&a).await;
    for blank in ["", "   "] {
        let r = send(&a, "POST", "/api/setup", REMOTE, Some(&normal), None, Some(setup_body(blank))).await;
        assert_eq!(r.status, StatusCode::OK, "{blank:?}: {}", r.body);
    }
    Ok(())
}
