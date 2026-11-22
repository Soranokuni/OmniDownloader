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

// ---- P13.2: everyday settings without a restart ----

/// The wizard harness with the daemon's live delivery paths: the watchfolder starts
/// at `<dir>/old-watch`, the archive at `<dir>/arch`.
fn live_app(timing: (u64, u64, u64)) -> Result<(App, omni_web::admin_delivery::LiveDelivery)> {
    let base = app(false)?;
    let install = omni_core::paths::AppPaths::with_root(base.dir.path(), "config.json");
    let mut archive_cfg = AppConfig::default().archive;
    archive_cfg.path = base.dir.path().join("arch").to_string_lossy().into_owned();
    let old = base.dir.path().join("old-watch");
    let archive = omni_core::archive::ArchiveSettings::from_config(&archive_cfg, &install, &old);
    let old_text = old.to_string_lossy().into_owned();
    let ms = std::time::Duration::from_millis;
    let live = omni_web::admin_delivery::LiveDelivery::new(omni_broadcast::pipeline::DeliveryPaths::new(old, Some(archive)), install)
        .with_timing(ms(timing.0), ms(timing.1), ms(timing.2));
    let state = base.state.clone().with_delivery(live.clone());
    {
        let mut cfg = state.config.try_write().unwrap();
        cfg.archive = archive_cfg;
        cfg.watchfolder_path = old_text;
    }
    Ok((App { router: WebServer::build_router(state.clone()), repo: base.repo.clone(), state, dir: base.dir }, live))
}

#[tokio::test]
async fn a_new_watchfolder_is_used_at_once_when_no_job_runs() -> Result<()> {
    let (app, live) = live_app((5_000, 100, 5_000))?;
    let tok = session_for(&app, "it@newsroom.local");
    let new = app.dir.path().join("new-watch");
    let r = send(&app, "POST", "/api/admin/settings/watchfolder", Some(&tok), Some(json!({"path": new.to_string_lossy()}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!((r.json()["restart_required"].as_bool(), r.json()["pending"].as_bool()), (Some(false), Some(false)), "{}", r.body);
    assert_eq!(live.paths.watchfolder(), new);
    let g = send(&app, "GET", "/api/admin/settings", Some(&tok), None).await;
    assert_eq!(g.json()["watchfolder_in_use"], new.to_string_lossy().as_ref());
    assert!(g.json()["watchfolder_pending"].is_null(), "{}", g.body);
    Ok(())
}

#[tokio::test]
async fn while_a_job_runs_the_new_watchfolder_waits_and_applies_when_it_ends() -> Result<()> {
    let (app, live) = live_app((100, 100, 400))?;
    let tok = session_for(&app, "it@newsroom.local");
    let old = live.paths.watchfolder();
    let new = app.dir.path().join("new-watch");
    let running = app.state.update_gate.try_enter().expect("a job holds a pass");

    let r = send(&app, "POST", "/api/admin/settings/watchfolder", Some(&tok), Some(json!({"path": new.to_string_lossy()}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!((r.json()["restart_required"].as_bool(), r.json()["pending"].as_bool()), (Some(false), Some(true)), "{}", r.body);
    assert_eq!(live.paths.watchfolder(), old, "not under a running job");
    let g = send(&app, "GET", "/api/admin/settings", Some(&tok), None).await;
    assert_eq!(g.json()["watchfolder_pending"]["path"], new.to_string_lossy().as_ref(), "{}", g.body);
    assert_eq!(g.json()["watchfolder_path"], new.to_string_lossy().as_ref(), "saved for the next start too");

    drop(running);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while live.paths.watchfolder() != new && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(live.paths.watchfolder(), new, "applied once the job ended");
    assert!(live.pending().is_none());
    Ok(())
}

/// Wait up to ten seconds for `done`.
async fn eventually(done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// P13.2b: B is saved while a job runs and drains; C replaces it before the
/// job ends. When it ends, B is never put into use, C is.
#[tokio::test]
async fn a_watchfolder_replaced_while_it_waited_is_never_used() -> Result<()> {
    let (app, live) = live_app((5_000, 50, 200))?;
    let tok = session_for(&app, "it@newsroom.local");
    let (b, c) = (app.dir.path().join("watch-b"), app.dir.path().join("watch-c"));
    let running = app.state.update_gate.try_enter().expect("a job holds a pass");

    for folder in [&b, &c] {
        let r = send(&app, "POST", "/api/admin/settings/watchfolder", Some(&tok), Some(json!({"path": folder.to_string_lossy()}))).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.json()["pending"].as_bool(), Some(true), "{}", r.body);
    }
    drop(running);
    eventually(|| live.paths.watchfolder() == c).await;
    assert_eq!(live.paths.watchfolder(), c, "the latest save is in use");
    eventually(|| live.pending().is_none()).await;
    assert!(live.pending().is_none());
    let audit: Vec<String> = app.repo.get_recent_audit_logs(50)?.into_iter().map(|a| a.message).collect();
    let in_use = |p: &std::path::Path| format!("Watchfolder {} in use", p.display());
    assert!(audit.iter().any(|m| m.starts_with(&in_use(&c))), "{audit:?}");
    assert!(!audit.iter().any(|m| m.starts_with(&in_use(&b))), "B was put into use: {audit:?}");
    Ok(())
}

/// P13.2b: only the first try holds new jobs back. Once its drain has timed
/// out, the queue runs normally and the change applies when no job runs.
#[tokio::test]
async fn after_the_first_drain_a_pending_watchfolder_holds_no_job_back() -> Result<()> {
    let (app, live) = live_app((200, 30, 100))?;
    let tok = session_for(&app, "it@newsroom.local");
    let new = app.dir.path().join("new-watch");
    let running = app.state.update_gate.try_enter().expect("a job holds a pass");
    let r = send(&app, "POST", "/api/admin/settings/watchfolder", Some(&tok), Some(json!({"path": new.to_string_lossy()}))).await;
    assert_eq!(r.json()["pending"].as_bool(), Some(true), "{}", r.body);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await; // the first drain is over

    let mut held_back = 0;
    for _ in 0..100 {
        match app.state.update_gate.try_enter() {
            Some(pass) => drop(pass),
            None => held_back += 1,
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(held_back <= 2, "new jobs were held back {held_back} times out of 100");
    assert_ne!(live.paths.watchfolder(), new, "not under the running job");

    drop(running);
    eventually(|| live.paths.watchfolder() == new).await;
    assert_eq!(live.paths.watchfolder(), new, "applied once no job runs");
    Ok(())
}

/// P13.3b (was an `/api/setup` test): the default relative `watchfolder`, typed
/// with spaces, is accepted, stored trimmed, and used under the install folder.
#[tokio::test]
async fn the_default_relative_watchfolder_is_accepted_and_stored_trimmed() -> Result<()> {
    let (app, live) = live_app((5_000, 100, 5_000))?;
    let tok = session_for(&app, "it@newsroom.local");
    let r = send(&app, "POST", "/api/admin/settings/watchfolder", Some(&tok), Some(json!({"path": " watchfolder "}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(app.state.config.read().await.watchfolder_path, "watchfolder");
    assert_eq!(AppConfig::load_from_file(app.dir.path().join("config.json"))?.watchfolder_path, "watchfolder");
    assert_eq!(live.paths.watchfolder(), app.dir.path().join("watchfolder"), "under the install folder");
    Ok(())
}

/// P13.3b: a relative watchfolder is tested under the daemon's install folder,
/// where delivery writes, never next to the running executable.
#[tokio::test]
async fn a_relative_watchfolder_is_tested_under_the_install_folder() -> Result<()> {
    let (app, _live) = live_app((5_000, 100, 5_000))?;
    let tok = session_for(&app, "it@newsroom.local");
    // A file, not a folder, takes the name in the install folder.
    std::fs::write(app.dir.path().join("watch-taken-test"), b"not a folder")?;
    let r = send(&app, "POST", "/api/admin/settings/watchfolder/test", Some(&tok), Some(json!({"path": "watch-taken-test"}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["ok"], false, "{}", r.body);
    assert!(r.json()["message"].as_str().unwrap_or("").contains("is not a folder"), "{}", r.body);
    Ok(())
}

#[tokio::test]
async fn a_watchfolder_inside_the_archive_is_refused() -> Result<()> {
    let (app, live) = live_app((5_000, 100, 5_000))?;
    let tok = session_for(&app, "it@newsroom.local");
    let before = app.state.config.read().await.watchfolder_path.clone();
    let inside = app.dir.path().join("arch").join("wf");
    let r = send(&app, "POST", "/api/admin/settings/watchfolder", Some(&tok), Some(json!({"path": inside.to_string_lossy()}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(r.body.contains("archive.path"), "{}", r.body);
    assert_eq!(app.state.config.read().await.watchfolder_path, before, "nothing saved");
    assert_eq!(live.paths.watchfolder(), app.dir.path().join("old-watch"));
    Ok(())
}

#[tokio::test]
async fn archive_settings_apply_at_once() -> Result<()> {
    let (app, live) = live_app((5_000, 100, 5_000))?;
    let tok = session_for(&app, "it@newsroom.local");
    assert!(live.paths.archive_writing().is_some());
    let r = send(&app, "POST", "/api/admin/settings/advanced", Some(&tok), Some(json!({"archive_enabled": false, "archive_retention_days": 30}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["restart_required"], false);
    assert!(live.paths.archive_writing().is_none(), "the next delivered job keeps no source");
    assert_eq!(live.paths.current().archive.unwrap().retention_days, 30);
    // A start-up setting still needs the restart.
    let r = send(&app, "POST", "/api/admin/settings/advanced", Some(&tok), Some(json!({"web_port": 8444}))).await;
    assert_eq!(r.json()["restart_required"], true);
    Ok(())
}

/// Counts the restart's two steps; `begin` fails when asked to.
struct FakeRestart {
    begun: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stopped: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    fail_begin: bool,
}

impl omni_web::admin_delivery::ServiceRestarter for FakeRestart {
    fn begin(&self) -> Result<(), String> {
        self.begun.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_begin { Err("helper refused (test)".into()) } else { Ok(()) }
    }
    fn stop(&self) -> Result<(), String> {
        self.stopped.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn the_restart_button_starts_the_helper_first_then_stops_and_is_audited() -> Result<()> {
    use std::sync::atomic::Ordering::SeqCst;
    let base = app(false)?;
    let tok = session_for(&base, "it@newsroom.local");
    // A console run has no service to restart.
    let r = send(&base, "POST", "/api/admin/restart-service", Some(&tok), Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);

    for fail_begin in [true, false] {
        let (begun, stopped): (std::sync::Arc<std::sync::atomic::AtomicUsize>, std::sync::Arc<std::sync::atomic::AtomicUsize>) =
            (Default::default(), Default::default());
        let fake = FakeRestart { begun: begun.clone(), stopped: stopped.clone(), fail_begin };
        let state = base.state.clone().with_restarter(std::sync::Arc::new(fake));
        let app = App { router: WebServer::build_router(state.clone()), repo: base.repo.clone(), state, dir: TempDir::new()? };
        let r = send(&app, "POST", "/api/admin/restart-service", Some(&tok), Some(json!({}))).await;
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert_eq!(begun.load(SeqCst), 1);
        if fail_begin {
            assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", r.body);
            assert_eq!(stopped.load(SeqCst), 0, "no helper, no stop: the service is never left stopped");
        } else {
            assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
            assert_eq!(stopped.load(SeqCst), 1);
            let audit: Vec<String> = base.repo.get_recent_audit_logs(50)?.into_iter().map(|a| a.message).collect();
            assert!(audit.iter().any(|m| m == "Service restart requested by it@newsroom.local"), "{audit:?}");
        }
    }
    Ok(())
}

/// P13.2b: two restart requests at once (a double click, two administrators)
/// start one helper and one stop.
#[tokio::test]
async fn two_restart_requests_start_one_helper() -> Result<()> {
    use std::sync::atomic::Ordering::SeqCst;
    let base = app(false)?;
    let tok = session_for(&base, "it@newsroom.local");
    let (begun, stopped): (std::sync::Arc<std::sync::atomic::AtomicUsize>, std::sync::Arc<std::sync::atomic::AtomicUsize>) =
        (Default::default(), Default::default());
    let fake = FakeRestart { begun: begun.clone(), stopped: stopped.clone(), fail_begin: false };
    let state = base.state.clone().with_restarter(std::sync::Arc::new(fake));
    let app = App { router: WebServer::build_router(state.clone()), repo: base.repo.clone(), state, dir: TempDir::new()? };
    let (a, b) = tokio::join!(
        send(&app, "POST", "/api/admin/restart-service", Some(&tok), Some(json!({}))),
        send(&app, "POST", "/api/admin/restart-service", Some(&tok), Some(json!({}))),
    );
    assert_eq!((a.status, b.status), (StatusCode::ACCEPTED, StatusCode::ACCEPTED), "{} / {}", a.body, b.body);
    let again = send(&app, "POST", "/api/admin/restart-service", Some(&tok), Some(json!({}))).await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.body);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!((begun.load(SeqCst), stopped.load(SeqCst)), (1, 1), "one helper, one stop");
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

// ---- P13.4: the test download and the journalists import ----

const WORKER: &str = "wizard-test-worker:1:1";

/// The worker's part for job `id`: leased, delivered into the watchfolder as
/// `file`, finished COMPLETED.
fn deliver(app: &App, id: i64, file: &str) -> Result<()> {
    assert_eq!(app.repo.lease_job(WORKER, 3600)?.expect("leased").id, id);
    assert!(app.repo.set_delivered_file(id, WORKER, file, 19.0)?);
    assert!(app.repo.finish(id, WORKER, omni_core::models::JobStatus::Completed, None, None, None)?);
    Ok(())
}

#[tokio::test]
async fn setup_is_complete_only_after_a_delivered_test_download_or_a_skip() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let complete = |body: Value| send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(body));

    // Neither: refused, nothing saved.
    let r = complete(json!({"mode": "beginner"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(r.body.contains("Δοκιμαστική λήψη"), "{}", r.body);
    assert!(!app.state.config.read().await.setup_completed);

    // The test download with the default link, as an ordinary job.
    let r = send(&app, "POST", "/api/admin/setup/test-download", Some(&tok), Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let id = r.json()["job_id"].as_i64().unwrap();
    let job = app.repo.get_job(id)?.unwrap();
    assert_eq!((job.url.as_str(), job.slug.as_str()), (omni_core::config::DEFAULT_SETUP_TEST_URL, "1_MCR_OMNIINGEST_TEST"));
    // Asked again while it runs: the same job, not a second one.
    let again = send(&app, "POST", "/api/admin/setup/test-download", Some(&tok), Some(json!({}))).await;
    assert_eq!((again.json()["job_id"].as_i64(), again.json()["new"].as_bool()), (Some(id), Some(false)), "{}", again.body);

    // Still running: no proof.
    let r = complete(json!({"mode": "beginner", "test_job_id": id})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    let s = send(&app, "GET", &format!("/api/admin/setup/test-download/{id}"), Some(&tok), None).await;
    assert_eq!((s.json()["status"].as_str(), s.json()["delivered"].as_bool()), (Some("PENDING"), Some(false)), "{}", s.body);
    assert!(s.json()["file_name"].is_null());

    // Delivered: the file name is shown and setup completes.
    let file = app.dir.path().join("watchfolder").join("1_MCR_OMNIINGEST_TEST.mxf");
    deliver(&app, id, &file.to_string_lossy())?;
    let s = send(&app, "GET", &format!("/api/admin/setup/test-download/{id}"), Some(&tok), None).await;
    assert_eq!(s.json()["delivered"].as_bool(), Some(true), "{}", s.body);
    assert_eq!(s.json()["file_name"].as_str(), Some("1_MCR_OMNIINGEST_TEST.mxf"), "only the name, not the path");
    let r = complete(json!({"mode": "beginner", "test_job_id": id})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(AppConfig::load_from_file(app.dir.path().join("config.json"))?.setup_completed);
    let audit: Vec<String> = app.repo.get_recent_audit_logs(20)?.into_iter().map(|a| a.message).collect();
    assert!(audit.iter().any(|m| m.contains(&format!("test download #{id} delivered as 1_MCR_OMNIINGEST_TEST.mxf"))), "{audit:?}");

    // A second test is a new job, not "already delivered".
    let r = send(&app, "POST", "/api/admin/setup/test-download", Some(&tok), Some(json!({}))).await;
    assert_eq!(r.json()["new"].as_bool(), Some(true), "{}", r.body);
    Ok(())
}

#[tokio::test]
async fn only_a_test_job_the_pipeline_delivered_proves_the_setup() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    // An ordinary job delivered by the pipeline is not the test download.
    let other = app
        .repo
        .enqueue(&omni_core::models::NewJob::new("https://video.news.example/v/1", "1_MCR_ASSET", "MCR"), 24)?
        .job_id();
    deliver(&app, other, "watchfolder/1_MCR_ASSET.mxf")?;
    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "beginner", "test_job_id": other}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    let s = send(&app, "GET", &format!("/api/admin/setup/test-download/{other}"), Some(&tok), None).await;
    assert_eq!(s.status, StatusCode::NOT_FOUND);

    // A test download that failed and that someone marked done by hand did not reach playout.
    let r = send(&app, "POST", "/api/admin/setup/test-download", Some(&tok), Some(json!({"url": "https://clips.news.example/test.mp4"}))).await;
    let id = r.json()["job_id"].as_i64().unwrap();
    assert_eq!(app.state.config.read().await.setup_test_url, "https://clips.news.example/test.mp4", "a changed link is kept");
    assert_eq!(app.repo.lease_job(WORKER, 3600)?.unwrap().id, id);
    // Its rename was recorded but the job still failed (say, the verify after it).
    assert!(app.repo.set_delivered_file(id, WORKER, "watchfolder/1_MCR_OMNIINGEST_TEST.mxf", 19.0)?);
    assert!(app.repo.finish(id, WORKER, omni_core::models::JobStatus::Failed, Some("DOWNLOAD_FAILED"), Some("test failure"), None)?);
    assert!(app.repo.mark_completed_manually(id)?);
    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "beginner", "test_job_id": id}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(!app.state.config.read().await.setup_completed);

    // A link that is not one is refused, with the key named.
    let r = send(&app, "POST", "/api/admin/setup/test-download", Some(&tok), Some(json!({"url": "javascript:alert(1)"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("setup_test_url"), "{}", r.body);
    Ok(())
}

// ---- P13.4b ----

/// A test job as the wizard makes it, with `slug` (an article renames `1_` to `1A_`).
fn test_job(app: &App, url: &str, slug: &str, parent: Option<i64>) -> Result<i64> {
    let mut j = omni_core::models::NewJob::new(url, slug, "MCR");
    j.keyword = "OMNIINGEST_TEST".into();
    j.notes = Some("Δοκιμαστική λήψη του οδηγού εγκατάστασης".into());
    j.parent_job_id = parent;
    Ok(app.repo.enqueue(&j, 0)?.job_id())
}

/// P13.4c: journalist and keyword alone do not make a test job; the note does.
#[tokio::test]
async fn a_delivered_job_without_the_test_note_proves_nothing() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let mut j = omni_core::models::NewJob::new("https://video.example/v/lookalike-test", "1_MCR_OMNIINGEST_TEST", "MCR");
    j.keyword = "OMNIINGEST_TEST".into();
    j.notes = Some("Βάλτε το στο δελτίο των 8".into());
    let id = app.repo.enqueue(&j, 0)?.job_id();
    deliver(&app, id, "watchfolder/1_MCR_OMNIINGEST_TEST.mxf")?;
    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "beginner", "test_job_id": id}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(!app.state.config.read().await.setup_completed);
    Ok(())
}

#[tokio::test]
async fn a_completed_test_job_with_no_delivered_file_proves_nothing() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let r = send(&app, "POST", "/api/admin/setup/test-download", Some(&tok), Some(json!({}))).await;
    let id = r.json()["job_id"].as_i64().unwrap();
    assert_eq!(app.repo.lease_job(WORKER, 3600)?.unwrap().id, id);
    // Finished COMPLETED (delivered_at is set) but no file was ever recorded.
    assert!(app.repo.finish(id, WORKER, omni_core::models::JobStatus::Completed, None, None, None)?);
    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "beginner", "test_job_id": id}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    let s = send(&app, "GET", &format!("/api/admin/setup/test-download/{id}"), Some(&tok), None).await;
    assert_eq!(s.json()["delivered"].as_bool(), Some(false), "{}", s.body);
    Ok(())
}

#[tokio::test]
async fn the_test_link_is_refused_while_a_newsroom_job_has_it() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let url = omni_core::config::DEFAULT_SETUP_TEST_URL;
    let newsroom = app.repo.enqueue(&omni_core::models::NewJob::new(url, "1_PAPADAKI_ZOO", "PAPADAKI"), 24)?.job_id();
    let r = send(&app, "POST", "/api/admin/setup/test-download", Some(&tok), Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(r.body.contains(&format!("job #{newsroom}")), "{}", r.body);
    let job = app.repo.get_job(newsroom)?.unwrap();
    assert_eq!((job.slug.as_str(), job.journalist.as_str()), ("1_PAPADAKI_ZOO", "PAPADAKI"), "the newsroom job is untouched");
    Ok(())
}

#[tokio::test]
async fn a_test_job_renamed_by_an_article_still_proves_and_its_sibling_does_not() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let renamed = test_job(&app, "https://news.example/article-test", "1A_MCR_OMNIINGEST_TEST", None)?;
    deliver(&app, renamed, "watchfolder/1A_MCR_OMNIINGEST_TEST.mxf")?;
    let sibling = test_job(&app, "https://video.example/v/sibling-test", "1B_MCR_OMNIINGEST_TEST", Some(renamed))?;
    deliver(&app, sibling, "watchfolder/1B_MCR_OMNIINGEST_TEST.mxf")?;

    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "beginner", "test_job_id": sibling}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "a sibling is not the test: {}", r.body);
    let s = send(&app, "GET", &format!("/api/admin/setup/test-download/{renamed}"), Some(&tok), None).await;
    assert_eq!(s.json()["file_name"].as_str(), Some("1A_MCR_OMNIINGEST_TEST.mxf"), "{}", s.body);
    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "beginner", "test_job_id": renamed}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    Ok(())
}

/// The race itself: the import planned to add NEOS from its snapshot; the
/// desk saved NEOS (and gave an address away) before the insert. The insert
/// adds nothing and changes nobody.
#[test]
fn the_import_insert_never_overwrites_a_person_saved_after_its_snapshot() -> Result<()> {
    use omni_core::repository::NewJournalist;
    let app = app(false)?;
    app.repo.save_journalist("NEOS", "Neos (desk)", &["desk@news.example".to_string()], 7)?;
    app.repo.save_journalist("TRITOS", "Tritos", &["allos@news.example".to_string()], 0)?;

    let r = app.repo.add_new_journalist("neos", "Neos Testakis", &["neos@news.example".to_string()])?;
    assert_eq!(r, NewJournalist::AlreadyListed);
    let r = app.repo.add_new_journalist("ALLOS", "Allos", &["Allos@News.example".to_string()])?;
    assert_eq!(r, NewJournalist::AddressTaken { address: "allos@news.example".into(), surname: "TRITOS".into() });
    let people = app.repo.list_journalists()?;
    let neos = people.iter().find(|j| j.surname == "NEOS").unwrap();
    assert_eq!((neos.full_name.as_str(), neos.emails.clone(), neos.default_priority), ("Neos (desk)", vec!["desk@news.example".to_string()], 7));
    assert!(people.iter().all(|j| j.surname != "ALLOS"));

    assert_eq!(app.repo.add_new_journalist("PEMPTOS", "Pemptos", &["pemptos@news.example".to_string()])?, NewJournalist::Added);
    Ok(())
}

#[tokio::test]
async fn a_person_saved_on_the_desk_between_preview_and_import_is_not_changed() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let text = "NEOS\tNeos Testakis\tneos@news.example\nALLOS\tAllos Testakis\tallos@news.example\n";
    let r = send(&app, "POST", "/api/admin/journalists/import", Some(&tok), Some(json!({"text": text, "dry_run": true}))).await;
    assert_eq!(r.json()["add"].as_array().map(|a| a.len()), Some(2), "{}", r.body);

    // Meanwhile on the desk: NEOS saved with other details, and ALLOS's address given to someone else.
    app.repo.save_journalist("NEOS", "Neos (desk)", &["desk@news.example".to_string()], 7)?;
    app.repo.save_journalist("TRITOS", "Tritos", &["allos@news.example".to_string()], 0)?;

    let r = send(&app, "POST", "/api/admin/journalists/import", Some(&tok), Some(json!({"text": text}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let people = app.repo.list_journalists()?;
    let neos = people.iter().find(|j| j.surname == "NEOS").unwrap();
    assert_eq!((neos.full_name.as_str(), neos.emails.clone(), neos.default_priority), ("Neos (desk)", vec!["desk@news.example".to_string()], 7));
    assert!(people.iter().all(|j| j.surname != "ALLOS"), "an address that is someone else's now is refused");
    Ok(())
}

#[tokio::test]
async fn pasted_journalists_are_previewed_then_added_and_people_on_the_list_are_kept() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    app.repo.save_journalist("PAPADAKI", "Anna Papadaki", &["anna@news.example".to_string()], 5)?;
    let before = app.repo.list_journalists()?.len();
    let text = "Επώνυμο\tΟνοματεπώνυμο\tEmail\nΠαπαδάκη\tΆλλη Άννα\tother@news.example\nΔοκιμάκης\tΝίκος Δοκιμάκης\tnikos@news.example\nKAKOS\tKakos\tnot-an-address\n";

    let r = send(&app, "POST", "/api/admin/journalists/import", Some(&tok), Some(json!({"text": text, "dry_run": true}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let v = r.json();
    assert_eq!(v["add"].as_array().map(|a| a.len()), Some(1), "{v}");
    assert_eq!(v["add"][0]["surname"], "DOKIMAKIS");
    assert_eq!(v["existing"][0]["surname"], "PAPADAKI");
    assert_eq!(v["problems"][0]["line"], 4);
    assert_eq!(app.repo.list_journalists()?.len(), before, "a preview adds nobody");

    let r = send(&app, "POST", "/api/admin/journalists/import", Some(&tok), Some(json!({"text": text}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let people = app.repo.list_journalists()?;
    let anna = people.iter().find(|j| j.surname == "PAPADAKI").unwrap();
    assert_eq!((anna.full_name.as_str(), anna.emails.clone(), anna.default_priority), ("Anna Papadaki", vec!["anna@news.example".to_string()], 5), "kept as it was");
    let nikos = people.iter().find(|j| j.surname == "DOKIMAKIS").expect("added");
    assert_eq!(nikos.emails, ["nikos@news.example"]);
    assert!(app.repo.data_dir().unwrap().join("backups").join("taxonomy").read_dir()?.next().is_some(), "a copy was kept first");
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

    // P13.4: with the test download skipped on purpose.
    let r = send(&app, "POST", "/api/admin/settings/complete", Some(&tok), Some(json!({"mode": "it", "skip_test": true}))).await;
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
    c.llm.endpoint = "http://llm.example/v1".into();
    c.llm.model = "model-test".into();

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

// ---- P13.8: the diagnostics package ----

#[tokio::test]
async fn the_diagnostics_package_downloads_with_no_stored_secret_in_it() -> Result<()> {
    use std::io::Read;
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let planted = "graph-store-secret-planted";
    app.state.secrets.set(omni_core::secrets::keys::GRAPH_CLIENT_SECRET, planted)?;
    // P13.8b: a cookie jar's value, echoed by a tool into the log.
    let jar_value = "jar-cookie-value-planted-0123";
    let jar = omni_core::cookies::parse_netscape("video.example", &format!(".video.example\tTRUE\t/\tTRUE\t1893456000\tsid\t{jar_value}\n"))?;
    app.repo.put_cookie_jar("video.example", &jar, "it@newsroom.local", chrono::Utc::now())?;
    let logs = app.dir.path().join("logs");
    std::fs::create_dir_all(&logs)?;
    std::fs::write(logs.join("omni-ingest.2026-11-21.log"), format!("INFO started\nWARN Graph refused {planted}\nDEBUG yt-dlp sent sid {jar_value}\n"))?;

    let req = Request::builder()
        .method("GET")
        .uri("/api/admin/diagnostics.zip")
        .header("cookie", format!("omni_session={tok}"))
        .body(Body::empty())?;
    let res = app.router.clone().layer(MockConnectInfo(LOOPBACK.parse::<SocketAddr>().unwrap())).oneshot(req).await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/zip");
    let disposition = res.headers()["content-disposition"].to_str()?.to_string();
    assert!(disposition.starts_with("attachment;") && disposition.contains("omni-diagnostics-"), "{disposition}");
    let bytes = res.into_body().collect().await?.to_bytes().to_vec();

    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut all = String::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        all.push_str(f.name());
        f.read_to_string(&mut all)?;
    }
    assert!(all.contains("logs/omni-ingest.2026-11-21.log") && all.contains("WARN Graph refused ***"), "{all}");
    assert!(!all.contains(planted), "the stored secret is in the package");
    assert!(!all.contains(jar_value) && all.contains("DEBUG yt-dlp sent sid ***"), "the jar value is in the package");
    // Written under the install's temp folder.
    assert_eq!(std::fs::read_dir(app.dir.path().join("temp").join("diagnostics"))?.count(), 1);
    let audit: Vec<String> = app.repo.get_recent_audit_logs(10)?.into_iter().map(|a| a.message).collect();
    assert!(audit.iter().any(|m| m.starts_with("Diagnostics package omni-diagnostics-")), "{audit:?}");
    Ok(())
}

/// P13.8b: while one package is being made a second is refused, and the
/// answer is a page a person reads (the panel links here directly), never a
/// JSON body the browser would save as a .zip.
#[tokio::test]
async fn a_second_package_while_one_is_made_is_refused_with_a_page() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let _busy = app.state.diagnostics_lock.clone().try_lock_owned().expect("free");
    let req = Request::builder()
        .method("GET")
        .uri("/api/admin/diagnostics.zip")
        .header("cookie", format!("omni_session={tok}"))
        .body(Body::empty())?;
    let res = app.router.clone().layer(MockConnectInfo(LOOPBACK.parse::<SocketAddr>().unwrap())).oneshot(req).await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert!(res.headers()["content-type"].to_str()?.starts_with("text/html"), "{:?}", res.headers());
    assert!(res.headers().get("content-disposition").is_none(), "not a download");
    let body = String::from_utf8(res.into_body().collect().await?.to_bytes().to_vec())?;
    assert!(body.contains("ετοιμάζεται ήδη") && body.contains("href=\"/admin\""), "{body}");
    assert!(!app.dir.path().join("temp").join("diagnostics").exists(), "nothing was written");

    // Signed out: also a page, not JSON.
    let req = Request::builder().method("GET").uri("/api/admin/diagnostics.zip").body(Body::empty())?;
    let res = app.router.clone().layer(MockConnectInfo(LOOPBACK.parse::<SocketAddr>().unwrap())).oneshot(req).await?;
    assert!(matches!(res.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN), "{}", res.status());
    assert!(res.headers()["content-type"].to_str()?.starts_with("text/html"));
    Ok(())
}

// ---- P13.5: the Settings section's operation form ----

#[tokio::test]
async fn the_operation_form_saves_in_range_values_and_names_a_bad_one() -> Result<()> {
    let app = app(false)?;
    let tok = session_for(&app, "it@newsroom.local");
    let g = send(&app, "GET", "/api/admin/settings", Some(&tok), None).await;
    assert_eq!(g.json()["operation"]["max_concurrent_downloads"], 3, "{}", g.body);

    let r = send(&app, "POST", "/api/admin/settings/operation", Some(&tok), Some(json!({
        "repair_mode": "shadow", "max_concurrent_downloads": 5, "retry_late_after_mins": 12,
        "graph_processed_folder": "Omni/Done",
    }))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["restart_required"], true, "repair mode and concurrency are read at start");
    let saved = AppConfig::load_from_file(app.dir.path().join("config.json"))?;
    assert_eq!((saved.max_concurrent_downloads, saved.retry.late_after_mins, saved.graph.processed_folder.as_str()), (5, 12, "Omni/Done"));
    assert_eq!(saved.repair.mode, omni_core::config::RepairMode::Shadow);
    assert_eq!(app.state.config.read().await.retry.late_after_mins, 12, "the late strip reads it now");

    let r = send(&app, "POST", "/api/admin/settings/operation", Some(&tok), Some(json!({ "max_concurrent_transcodes": 20 }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("max_concurrent_transcodes = 20 is outside 1..8"), "{}", r.body);
    assert_eq!(AppConfig::load_from_file(app.dir.path().join("config.json"))?.max_concurrent_transcodes, 2, "nothing saved");
    Ok(())
}
