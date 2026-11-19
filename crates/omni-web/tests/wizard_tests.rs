//! The first-setup wizard's backend (P7.12): the account swap and the settings
//! endpoints. Route policy for all of them is also in auth_matrix_tests.rs.

use std::net::SocketAddr;

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::AppState;

const NORMAL: &str = "correct-horse-battery";
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
}

/// `with_default`: the first-run account exists; otherwise only an ordinary admin.
fn app(with_default: bool) -> Result<App> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    if with_default {
        repo.create_user("admin", "Admin", UserRole::Admin, "First-run administrator", None)?;
    } else {
        repo.create_user("it@newsroom.local", NORMAL, UserRole::Admin, "Administrator", None)?;
    }
    repo.create_user("reporter@newsroom.local", NORMAL, UserRole::User, "Reporter", None)?;
    let mut config = AppConfig::default();
    config.security.login_rate_limit_per_5min = 10_000;
    config.security.login_rate_limit_per_account_hour = 10_000;
    config.save_to_file(&dir.path().join("config.json"))?;
    let state = AppState::new(repo.clone(), config, dir.path().join("config.json"))
        .with_secret_store(omni_core::secrets::SecretStore::new(dir.path().join("secrets.bin")));
    Ok(App { router: WebServer::build_router(state.clone()), repo, state, dir })
}

async fn send(app: &App, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("cookie", format!("omni_session={t}"));
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
        None => Body::empty(),
    };
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo(LOOPBACK.parse::<SocketAddr>().unwrap()))
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let cookie = res.headers().get("set-cookie").and_then(|v| v.to_str().ok()).map(str::to_string);
    let body = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    Reply { status, cookie, body }
}

fn session_for(app: &App, login: &str) -> String {
    let id = app.repo.get_user_by_email(login).unwrap().unwrap().id;
    let token = omni_core::auth::generate_session_token();
    app.repo.create_session(id, &token, 1).unwrap();
    token
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

fn account(login: &str, password: &str) -> Value {
    json!({ "full_name": "Test Operator", "login": login, "password": password })
}

// ---- the account step ----

#[tokio::test]
async fn the_account_step_switches_the_session_from_the_default_admin_to_the_new_one() -> Result<()> {
    let app = app(true)?;
    assert!(app.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    let old = session_for(&app, "admin");

    let r = send(&app, "POST", "/api/setup/wizard/account", Some(&old), Some(account("station.admin", "a-long-passphrase-1"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let new = token_of(&r);

    // The old cookie is dead, the new one is a real administrator.
    let gone = send(&app, "GET", "/api/auth/me", Some(&old), None).await;
    assert_eq!(gone.status, StatusCode::UNAUTHORIZED);
    let me = send(&app, "GET", "/api/auth/me", Some(&new), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["email"], "station.admin");
    assert_eq!(me.json()["password_change_required"], false);
    let settings = send(&app, "GET", "/api/admin/settings", Some(&new), None).await;
    assert_eq!(settings.status, StatusCode::OK, "{}", settings.body);

    // The default account is off and no longer live.
    assert!(app.repo.get_user_by_email("admin")?.is_none());
    assert!(!app.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    Ok(())
}

#[tokio::test]
async fn a_failed_create_leaves_the_default_account_live() -> Result<()> {
    let app = app(true)?;
    let old = session_for(&app, "admin");
    // A taken login, a weak password, "admin", a bad login: each refused.
    for body in [
        account("reporter@newsroom.local", "a-long-passphrase-1"),
        account("station.admin", "short"),
        account("Admin", "a-long-passphrase-1"),
        account("ab", "a-long-passphrase-1"),
        account("two words", "a-long-passphrase-1"),
        json!({ "full_name": " ", "login": "station.admin", "password": "a-long-passphrase-1" }),
    ] {
        let r = send(&app, "POST", "/api/setup/wizard/account", Some(&old), Some(body.clone())).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}: {}", r.body);
    }
    assert!(app.repo.get_user_by_email("admin")?.is_some(), "the default account must stay active");
    assert!(app.state.default_admin_live.load(std::sync::atomic::Ordering::Relaxed));
    let me = send(&app, "GET", "/api/auth/me", Some(&old), None).await;
    assert_eq!(me.status, StatusCode::OK, "the default session must survive a refused step");
    assert_eq!(me.json()["password_change_required"], true);
    Ok(())
}

#[tokio::test]
async fn an_ordinary_admin_cannot_use_the_account_step() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let r = send(&app, "POST", "/api/setup/wizard/account", Some(&tok), Some(account("another.admin", "a-long-passphrase-1"))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(app.repo.get_user_by_email("another.admin")?.is_none(), "nothing may be created");
    Ok(())
}

#[tokio::test]
async fn a_default_session_is_refused_the_settings_endpoints() -> Result<()> {
    let app = app(true)?;
    let tok = session_for(&app, "admin");
    for (m, u) in [("GET", "/api/admin/settings"), ("GET", "/api/admin/settings/answer-file")] {
        let r = send(&app, m, u, Some(&tok), None).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{u}");
    }
    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "beginner"}))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    Ok(())
}

// ---- watchfolder ----

#[tokio::test]
async fn the_watchfolder_test_refuses_bad_paths_and_passes_on_a_temp_dir() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let test = |p: String| json!({ "path": p });

    // Blank and drive-relative paths are refused with the operator-facing reason.
    for bad in ["", "   ", "Z:folder"] {
        let r = send(&app, "POST", "/api/admin/settings/watchfolder/test", Some(&tok), Some(test(bad.into()))).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.json()["ok"], false, "{bad:?}: {}", r.body);
        assert!(!r.json()["message"].as_str().unwrap().is_empty());
    }

    // A folder that does not exist yet is created, and the probe leaves nothing behind.
    let target = app.dir.path().join("ingest").join("drop");
    let r = send(&app, "POST", "/api/admin/settings/watchfolder/test", Some(&tok), Some(test(target.to_string_lossy().into()))).await;
    assert_eq!(r.json()["ok"], true, "{}", r.body);
    assert!(target.is_dir());
    assert_eq!(std::fs::read_dir(&target)?.count(), 0, "the probe file must be removed");
    assert!(r.json()["account"].as_str().is_some());
    Ok(())
}

#[tokio::test]
async fn saving_the_watchfolder_needs_a_passing_test_and_reports_a_restart() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let before = app.state.config.read().await.watchfolder_path.clone();

    let r = send(&app, "POST", "/api/admin/settings/watchfolder", Some(&tok), Some(json!({"path": "Z:folder"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert_eq!(app.state.config.read().await.watchfolder_path, before, "a refused path must not be saved");

    let good = app.dir.path().join("wf");
    let want = format!("  {}  ", good.display());
    let r = send(&app, "POST", "/api/admin/settings/watchfolder", Some(&tok), Some(json!({"path": want}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["restart_required"], true);
    let saved = AppConfig::load_from_file(app.dir.path().join("config.json"))?;
    assert_eq!(saved.watchfolder_path, good.to_string_lossy());
    Ok(())
}

// ---- mail ----

#[tokio::test]
async fn the_mail_secret_goes_to_the_store_and_is_never_returned() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let secret = "very-secret-value-123";
    let r = send(
        &app,
        "POST",
        "/api/admin/settings/mail",
        Some(&tok),
        Some(json!({"tenant_id": "t-1", "client_id": "c-1", "mailbox": "ingest@station.example", "client_secret": secret})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["secret_set"], true);
    assert!(!r.body.contains(secret));
    assert_eq!(app.state.secrets.get(omni_core::secrets::keys::GRAPH_CLIENT_SECRET)?.as_deref(), Some(secret));
    let file = std::fs::read_to_string(app.dir.path().join("config.json"))?;
    assert!(!file.contains(secret), "the secret must never reach config.json");

    // An empty secret keeps the stored one, and no read endpoint shows it.
    let r = send(
        &app,
        "POST",
        "/api/admin/settings/mail",
        Some(&tok),
        Some(json!({"tenant_id": "t-2", "client_id": "c-1", "mailbox": "ingest@station.example", "client_secret": ""})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(app.state.secrets.get(omni_core::secrets::keys::GRAPH_CLIENT_SECRET)?.as_deref(), Some(secret));
    for uri in ["/api/admin/settings", "/api/admin/settings/answer-file"] {
        let g = send(&app, "GET", uri, Some(&tok), None).await;
        assert!(!g.body.contains(secret), "{uri}");
    }
    let g = send(&app, "GET", "/api/admin/settings", Some(&tok), None).await;
    assert_eq!(g.json()["graph"]["secret_set"], true);
    assert_eq!(g.json()["graph"]["tenant_id"], "t-2");

    // Half a mailbox is refused.
    let r = send(&app, "POST", "/api/admin/settings/mail", Some(&tok), Some(json!({"tenant_id": "t", "client_id": "", "mailbox": ""}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    Ok(())
}

/// P11.11c/d: in the daemon, saved mail settings are what the watcher reads with from
/// the next poll, with no restart; the secret's expiry date is kept and checked.
#[tokio::test]
async fn saved_mail_settings_reach_the_running_watcher_without_a_restart() -> Result<()> {
    use omni_email::watcher::{LiveMail, MailSettings};
    let base = app(false)?;
    let live = LiveMail::new(MailSettings::graph(&AppConfig::default().graph));
    let state = base.state.clone().with_mail(live.clone());
    let app = App { router: WebServer::build_router(state.clone()), repo: base.repo.clone(), state, dir: base.dir };
    let tok = session_for(&app, "it@newsroom.local");
    let mail = |expires: &str| {
        json!({"tenant_id": "t-1", "client_id": "c-1", "mailbox": "ingest@station.example",
               "client_secret": "live-secret-1", "client_secret_expires": expires})
    };

    let r = send(&app, "POST", "/api/admin/settings/mail", Some(&tok), Some(mail("31/01/2027"))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("graph.client_secret_expires") && r.body.contains("YYYY-MM-DD"), "{}", r.body);
    assert!(!live.current().source.is_configured(), "a refused save changes nothing");

    let r = send(&app, "POST", "/api/admin/settings/mail", Some(&tok), Some(mail("2027-01-31"))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["restart_required"], false);
    let now = live.current();
    assert!(now.source.is_configured());
    assert_eq!(now.source.describe(), "graph ingest@station.example");
    assert_eq!(now.secret_expires, chrono::NaiveDate::from_ymd_opt(2027, 1, 31));
    let saved = AppConfig::load_from_file(app.dir.path().join("config.json"))?;
    assert_eq!(saved.graph.client_secret_expires, chrono::NaiveDate::from_ymd_opt(2027, 1, 31));
    let g = send(&app, "GET", "/api/admin/settings", Some(&tok), None).await;
    assert_eq!(g.json()["graph"]["client_secret_expires"], "2027-01-31");
    // The date belongs to this machine's secret: not in the answer file.
    let a = send(&app, "GET", "/api/admin/settings/answer-file", Some(&tok), None).await;
    assert!(a.json()["graph"].get("client_secret_expires").is_none(), "{}", a.body);

    // Absent keeps the date; blank clears it.
    let mut keep = mail("");
    keep.as_object_mut().unwrap().remove("client_secret_expires");
    send(&app, "POST", "/api/admin/settings/mail", Some(&tok), Some(keep)).await;
    assert_eq!(live.current().secret_expires, chrono::NaiveDate::from_ymd_opt(2027, 1, 31));
    let r = send(&app, "POST", "/api/admin/settings/mail", Some(&tok), Some(mail(""))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(live.current().secret_expires, None);
    assert_eq!(app.state.config.read().await.graph.client_secret_expires, None);
    Ok(())
}

/// Review round 2: a client secret set on the Credentials card reaches the running
/// watcher too, from its next check.
#[tokio::test]
async fn a_secret_set_on_the_credentials_card_reaches_the_running_watcher() -> Result<()> {
    use omni_email::watcher::{LiveMail, MailSettings};
    let base = app(false)?;
    let live = LiveMail::new(MailSettings::graph(&AppConfig::default().graph));
    let state = base.state.clone().with_mail(live.clone());
    let app = App { router: WebServer::build_router(state.clone()), repo: base.repo.clone(), state, dir: base.dir };
    let tok = session_for(&app, "it@newsroom.local");
    let r = send(&app, "POST", "/api/admin/settings/mail", Some(&tok),
        Some(json!({"tenant_id": "t-1", "client_id": "c-1", "mailbox": "ingest@station.example"}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!live.current().source.is_configured(), "no secret yet");

    let r = send(&app, "POST", "/api/secrets", Some(&tok), Some(json!({"key": "graph.client_secret", "value": "card-secret-1"}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(live.current().source.is_configured(), "the watcher reads with the new secret");
    Ok(())
}

// ---- advanced, complete, answer file ----

#[tokio::test]
async fn advanced_settings_are_validated_and_say_what_needs_a_restart() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let post = |b: Value| send(&app, "POST", "/api/admin/settings/advanced", Some(&tok), Some(b));

    for bad in [
        json!({"web_port": 0}),
        json!({"web_port": 70000}),
        json!({"mcr_open_networks": ["not-a-network"]}),
        json!({"trusted_proxies": ["10.0.0.0/99"]}),
        json!({"metrics_allow_networks": ["x"]}),
        json!({"tls_cert_path": "C:\\no\\such\\cert.pfx"}),
        json!({"archive_retention_days": 0}),
        json!({"allowed_hosts": ["http://bad/host"]}),
        json!({"trust_proxy_header": true}),
    ] {
        let r = post(bad.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}: {}", r.body);
    }
    // Nothing from the refused requests was saved.
    assert_eq!(app.state.config.read().await.web_port, 8080);

    // Live-only change: no restart.
    let r = post(json!({"mcr_open_networks": ["10.20.0.0/16", ""], "trusted_proxies": ["10.1.1.1/32"], "trust_proxy_header": true})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["restart_required"], false);
    assert_eq!(app.state.config.read().await.security.mcr_open_networks, vec!["10.20.0.0/16"]);

    // A start-up setting: restart.
    let r = post(json!({"web_port": 8443, "archive_enabled": false, "archive_retention_days": 30})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["restart_required"], true);
    let saved = AppConfig::load_from_file(app.dir.path().join("config.json"))?;
    assert_eq!(saved.web_port, 8443);
    assert!(!saved.archive.enabled);
    assert_eq!(saved.archive.retention_days, 30);

    // A file that is not a PKCS#12 bundle (a .cer, a .pem, or a wrong passphrase) is refused:
    // saved, it would stop the web listener at the next start. Nothing is stored.
    let pfx = app.dir.path().join("site.pfx");
    std::fs::write(&pfx, b"not really a certificate")?;
    let r = post(json!({"tls_cert_path": pfx.to_string_lossy(), "tls_password": "pfx-pass-9"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(!r.body.contains("pfx-pass-9"));
    // P13.3: one sentence after another, not a run of source indentation.
    assert!(r.body.contains("PKCS#12") && !r.body.contains("  "), "{}", r.body);
    assert!(app.state.secrets.get(omni_core::secrets::keys::TLS_PASSWORD)?.is_none(), "a refused passphrase must not be stored");
    assert!(app.state.config.read().await.tls.cert_path.is_none(), "a refused certificate must not be saved");
    assert!(!std::fs::read_to_string(app.dir.path().join("config.json"))?.contains("site.pfx"));
    Ok(())
}

#[tokio::test]
async fn completing_setup_records_the_mode_and_the_answer_file_has_no_secret() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    assert!(!app.state.config.read().await.setup_completed);

    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "expert"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(!app.state.config.read().await.setup_completed);

    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "it"}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let saved = AppConfig::load_from_file(app.dir.path().join("config.json"))?;
    assert!(saved.setup_completed);
    assert_eq!(saved.setup_mode, "it");

    send(&app, "POST", "/api/admin/settings/mail", Some(&tok), Some(json!({"tenant_id":"t","client_id":"c","mailbox":"m@x.example","client_secret":"zzz-secret"}))).await;
    let a = send(&app, "GET", "/api/admin/settings/answer-file", Some(&tok), None).await;
    assert_eq!(a.status, StatusCode::OK);
    let doc = a.json();
    assert_eq!(doc["setup_mode"], "it");
    assert_eq!(doc["graph"]["mailbox"], "m@x.example");
    assert!(doc["graph"].get("secret_set").is_none());
    assert!(!a.body.contains("zzz-secret"));
    assert!(!a.body.to_lowercase().contains("password"));
    Ok(())
}

/// Every leaf of a JSON document with its dotted path.
fn leaves(v: &Value, prefix: &str, out: &mut Vec<(String, Value)>) {
    match v.as_object() {
        Some(m) => {
            for (k, v) in m {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                leaves(v, &key, out);
            }
        }
        None => out.push((prefix.to_string(), v.clone())),
    }
}

/// P13.1b: what the admin page exports, Setup reads back. A key added to the
/// export without an import mapping (or a mapping dropped) fails here.
#[tokio::test]
async fn the_answer_file_reads_back_into_every_exported_setting() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let export = |cfg: AppConfig| {
        let (app, tok) = (&app, tok.clone());
        async move {
            *app.state.config.write().await = cfg;
            let r = send(app, "GET", "/api/admin/settings/answer-file", Some(&tok), None).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.body);
            r.json()
        }
    };

    let mut c = AppConfig::default();
    c.setup_completed = true;
    c.setup_mode = "it".into();
    c.watchfolder_path = r"\\dalet-test\ingest".into();
    c.web_port = 8443;
    c.security.mcr_open_networks = vec!["10.20.0.0/16".into()];
    c.archive.enabled = false;
    c.archive.retention_days = 30;
    c.tls.cert_path = Some(r"C:\certs\panel.pfx".into());
    c.security.trust_proxy_header = true;
    c.security.trusted_proxies = vec!["10.0.0.5/32".into()];
    c.security.allowed_hosts = vec!["ingest.newsroom.example".into()];
    c.metrics.enabled = true;
    c.metrics.allow_networks = vec!["10.9.0.0/24".into()];
    c.graph.tenant_id = "tenant-test".into();
    c.graph.client_id = "client-test".into();
    c.graph.mailbox = "ingest@newsroom.example".into();
    c.graph.client_secret = "zzz-graph-secret".into();
    c.llm.mode = omni_core::config::LlmMode::Off;
    c.llm.provider = "openai".into();
    c.ollama_endpoint = "http://llm.example/v1".into();
    c.ollama_model = "model-test".into();

    let defaults = export(AppConfig::default()).await;
    let doc = export(c).await;
    let (mut set, mut default_leaves) = (Vec::new(), Vec::new());
    leaves(&doc, "", &mut set);
    leaves(&defaults, "", &mut default_leaves);
    // The test sets every exported setting away from its default, so a missing mapping shows.
    for (key, value) in &set {
        if key == "answer_file_version" {
            continue;
        }
        let default = default_leaves.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        assert_ne!(Some(value), default, "set {key} to a non-default value in this test");
    }

    let applied = omni_core::answer_file::apply(&doc, AppConfig::default())?;
    assert!(!applied.config.setup_completed, "a new installation starts with the wizard pending");
    assert!(applied.config.graph.client_secret.is_empty());
    assert_eq!(applied.notes, [omni_core::answer_file::TLS_NOTE]);
    let back = export(applied.config).await;
    let mut back_leaves = Vec::new();
    leaves(&back, "", &mut back_leaves);
    for (key, value) in &set {
        let got = back_leaves.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        if key == "setup_completed" {
            assert_eq!(got, Some(&json!(false)));
        } else if key == "tls.cert_path" {
            // Not applied on purpose: the certificate and its passphrase are not on the
            // new machine, and a panel that cannot start leaves the wizard unreachable.
            assert_eq!(got, Some(&Value::Null));
        } else {
            assert_eq!(got, Some(value), "{key} did not survive the answer file");
        }
    }
    Ok(())
}

#[tokio::test]
async fn two_concurrent_account_steps_create_exactly_one_administrator() -> Result<()> {
    let app = app(true)?;
    let old = session_for(&app, "admin");
    let (a, b) = tokio::join!(
        send(&app, "POST", "/api/setup/wizard/account", Some(&old), Some(account("first.admin", "a-long-passphrase-1"))),
        send(&app, "POST", "/api/setup/wizard/account", Some(&old), Some(account("second.admin", "a-long-passphrase-1"))),
    );
    let ok = [&a, &b].iter().filter(|r| r.status == StatusCode::OK).count();
    assert_eq!(ok, 1, "{} / {}", a.body, b.body);
    let admins = app
        .repo
        .list_users()?
        .into_iter()
        .filter(|u| u.is_active && u.role == UserRole::Admin)
        .count();
    assert_eq!(admins, 1, "exactly one working administrator");
    assert!(app.repo.get_user_by_email("admin")?.is_none());
    Ok(())
}

#[tokio::test]
async fn the_settings_report_where_the_default_watchfolder_resolves() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let g = send(&app, "GET", "/api/admin/settings", Some(&tok), None).await;
    let resolved = g.json()["watchfolder_resolved"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&resolved).is_absolute(), "{resolved}");
    Ok(())
}
