//! LLM settings from the admin panel (plan P4.22): saved, applied without a
//! restart, the key kept out of config.json, and a stored key never sent to
//! a base URL other than the one it was saved with.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::AppState;

type Seen = Arc<Mutex<Vec<Option<String>>>>;

/// An OpenAI-compatible server that records the Authorization it was sent.
async fn provider() -> (Seen, String) {
    let seen: Seen = Arc::default();
    async fn chat(State(seen): State<Seen>, headers: HeaderMap, Json(_): Json<Value>) -> Json<Value> {
        seen.lock().unwrap().push(headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from));
        Json(json!({"choices":[{"message":{"content":"{\"word\":\"LIMANI\"}"},"finish_reason":"stop"}]}))
    }
    async fn models(State(seen): State<Seen>, headers: HeaderMap) -> Json<Value> {
        seen.lock().unwrap().push(headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from));
        Json(json!({"data":[{"id":"google/gemma-4-e4b"}]}))
    }
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(models))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service()).await.unwrap() });
    (seen, format!("http://{addr}/v1"))
}

struct App {
    dir: TempDir,
    state: AppState,
    router: Router,
    token: String,
}

fn app() -> Result<App> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let admin = repo.create_user("it@station.test", "correct-horse-battery", UserRole::Admin, "Admin", None)?;
    let token = omni_core::auth::generate_session_token();
    repo.create_session(admin, &token, 1)?;
    let store = omni_core::secrets::SecretStore::new(dir.path().join("secrets.bin"));
    let config_path = dir.path().join("config.json");
    AppConfig::default().save_to_file(&config_path)?;
    let state = AppState::new(repo, AppConfig::default(), config_path).with_secret_store(store);
    let router = WebServer::build_router(state.clone());
    Ok(App { dir, state, router, token })
}

async fn call(app: &App, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-omni-request", "1")
        .header("cookie", format!("omni_session={}", app.token));
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo("127.0.0.1:1".parse::<SocketAddr>().unwrap()))
        .oneshot(b.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn settings(base: &str, key: &str) -> Value {
    json!({
        "mode": "assist", "provider": "custom", "base_url": base, "model": "google/gemma-4-e4b",
        "auth": "bearer", "api_key": key, "timeout_secs": 10, "max_tokens": 200, "disable_thinking": true
    })
}

#[tokio::test]
async fn saving_applies_at_once_and_the_key_lives_only_in_the_store() -> Result<()> {
    let (_seen, base) = provider().await;
    let app = app()?;
    let (status, body) = call(&app, "POST", "/api/admin/llm", Some(settings(&base, "sk-station-key"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Applied: the live assist the watcher uses now points at the new server.
    assert!(app.state.llm.current().describe().contains(base.trim_start_matches("http://")), "{}", app.state.llm.current().describe());

    let written = std::fs::read_to_string(app.dir.path().join("config.json"))?;
    assert!(!written.contains("sk-station-key"), "the key reached config.json");
    assert!(written.contains(&base) && written.contains("\"auth\": \"bearer\""), "{written}");
    assert_eq!(app.state.secrets.get("llm.api_key")?.as_deref(), Some("sk-station-key"));

    let (_, got) = call(&app, "GET", "/api/admin/llm", None).await;
    assert_eq!(got["key_set"], true);
    assert!(!got.to_string().contains("sk-station-key"), "the API handed the key back: {got}");
    Ok(())
}

#[tokio::test]
async fn a_stored_key_is_sent_only_to_the_base_url_it_was_saved_with() -> Result<()> {
    let (seen_a, base_a) = provider().await;
    let (seen_b, base_b) = provider().await;
    let app = app()?;
    call(&app, "POST", "/api/admin/llm", Some(settings(&base_a, "sk-station-key"))).await;
    seen_a.lock().unwrap().clear();

    // Blank key, same URL: the stored key is used.
    let (status, body) = call(&app, "POST", "/api/admin/llm/test", Some(settings(&base_a, ""))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["answer"]["word"], "LIMANI");
    assert_eq!(seen_a.lock().unwrap().last().cloned().flatten().as_deref(), Some("Bearer sk-station-key"));

    // Blank key, another URL (typed in, perhaps someone else's server): no key.
    call(&app, "POST", "/api/admin/llm/models", Some(settings(&base_b, ""))).await;
    call(&app, "POST", "/api/admin/llm/test", Some(settings(&base_b, ""))).await;
    assert!(seen_b.lock().unwrap().iter().all(|a| a.is_none()), "the stored key went to another server: {:?}", seen_b.lock().unwrap());

    // Saving a different provider does not carry the old key over.
    call(&app, "POST", "/api/admin/llm", Some(settings(&base_b, ""))).await;
    assert_eq!(app.state.secrets.get("llm.api_key")?, None);
    Ok(())
}

#[tokio::test]
async fn bad_settings_are_refused_with_a_reason() -> Result<()> {
    let app = app()?;
    for (base, needle) in [
        ("http://api.openai.com/v1", "https"),
        ("not a url", "not a URL"),
        ("ftp://127.0.0.1/v1", "http"),
    ] {
        let (status, body) = call(&app, "POST", "/api/admin/llm", Some(settings(base, "k"))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{base}: {body}");
        assert!(body.to_string().contains(needle), "{base}: {body}");
    }
    let mut no_model = settings("http://127.0.0.1:1/v1", "");
    no_model["model"] = json!("");
    let (status, _) = call(&app, "POST", "/api/admin/llm", Some(no_model)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}


/// Plan P2.4: accounts can be deactivated from the panel, never the last
/// administrator or your own, and a deactivated account is signed out.
#[tokio::test]
async fn deactivating_an_account_signs_it_out_and_the_last_admin_is_protected() -> Result<()> {
    let app = app()?;
    let me = app.state.repo.list_users()?.into_iter().find(|u| u.email == "it@station.test").unwrap();
    let (status, body) = call(&app, "POST", &format!("/api/admin/users/{}/active", me.id), Some(json!({"active": false}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let other = app.state.repo.create_user("desk@station.test", "correct-horse-battery", UserRole::Admin, "Desk", None)?;
    let token = omni_core::auth::generate_session_token();
    app.state.repo.create_session(other, &token, 1)?;
    let (status, _) = call(&app, "POST", &format!("/api/admin/users/{other}/active"), Some(json!({"active": false}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.state.repo.delete_sessions_for_user(other)?, 0, "the deactivated account kept a session");
    assert!(!app.state.repo.get_user_by_id(other)?.unwrap().is_active);

    // Now "me" is the last active admin.
    let (status, body) = call(&app, "POST", &format!("/api/admin/users/{}/active", me.id), Some(json!({"active": false}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = call(&app, "POST", &format!("/api/admin/users/{other}/active"), Some(json!({"active": true}))).await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}
