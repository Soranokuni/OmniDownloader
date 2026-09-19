use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{sse::Event, IntoResponse, Redirect, Response, Sse};
use axum::Json;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use omni_core::auth::{generate_session_token, verify_password};
use omni_core::config::AuthMode;
use omni_core::dependencies::DependencyManager;
use omni_core::models::{JobStatus, User, UserRole};

use crate::assets::EmbeddedFile;
use crate::auth::{build_logout_cookie, build_session_cookie, extract_session_token, get_authenticated_user};
use crate::state::AppState;

// ==========================================
// Web Page Views
// ==========================================

pub async fn handle_root(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let auth_mode = { state.config.read().await.auth_mode.clone() };
    if let Some(user) = get_authenticated_user(&headers, &state.repo) {
        match user.role {
            UserRole::Admin => Redirect::temporary("/admin").into_response(),
            UserRole::OpenMcr => Redirect::temporary("/mcr").into_response(),
            UserRole::User => Redirect::temporary("/user").into_response(),
        }
    } else if auth_mode == AuthMode::OpenMcr {
        Redirect::temporary("/mcr").into_response()
    } else {
        Redirect::temporary("/login").into_response()
    }
}

pub async fn view_login() -> impl IntoResponse {
    EmbeddedFile("login.html")
}

pub async fn view_user(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if get_authenticated_user(&headers, &state.repo).is_some() {
        EmbeddedFile("user.html").into_response()
    } else {
        Redirect::temporary("/login").into_response()
    }
}

pub async fn view_mcr(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let auth_mode = { state.config.read().await.auth_mode.clone() };
    if auth_mode == AuthMode::OpenMcr || get_authenticated_user(&headers, &state.repo).is_some() {
        EmbeddedFile("mcr.html").into_response()
    } else {
        Redirect::temporary("/login").into_response()
    }
}

pub async fn view_admin(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Some(user) = get_authenticated_user(&headers, &state.repo) {
        if user.role == UserRole::Admin {
            return EmbeddedFile("admin.html").into_response();
        }
    }
    Redirect::temporary("/login").into_response()
}

pub async fn view_setup() -> impl IntoResponse {
    EmbeddedFile("setup.html")
}

// ==========================================
// Authentication API
// ==========================================

#[derive(Deserialize)]
pub struct LoginPayload {
    email: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    status: String,
    role: String,
    user: User,
}

pub async fn api_login(
    State(state): State<AppState>,
    Json(payload): Json<LoginPayload>,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let user = state
        .repo
        .get_user_by_email(&payload.email)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Invalid email or password"}))))?;

    if !verify_password(&payload.password, &user.password_hash) {
        return Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Invalid email or password"}))));
    }

    let token = generate_session_token();
    state
        .repo
        .create_session(user.id, &token, 14)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    let cookie_header = build_session_cookie(&token, 14);
    let role = user.role.as_str().to_string();

    let mut response = Json(LoginResponse {
        status: "ok".into(),
        role,
        user,
    })
    .into_response();

    response.headers_mut().insert(SET_COOKIE, cookie_header);
    Ok(response)
}

pub async fn api_logout(headers: HeaderMap, State(state): State<AppState>) -> impl IntoResponse {
    if let Some(token) = extract_session_token(&headers) {
        let _ = state.repo.delete_session(&token);
    }
    let mut response = Json(serde_json::json!({"status": "logged_out"})).into_response();
    response.headers_mut().insert(SET_COOKIE, build_logout_cookie());
    response
}

pub async fn api_me(headers: HeaderMap, State(state): State<AppState>) -> Result<Json<User>, StatusCode> {
    if let Some(user) = get_authenticated_user(&headers, &state.repo) {
        Ok(Json(user))
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ==========================================
// Jobs API
// ==========================================

#[derive(Deserialize)]
pub struct JobsQuery {
    my: Option<bool>,
    journalist: Option<String>,
}

pub async fn api_get_jobs(
    headers: HeaderMap,
    Query(query): Query<JobsQuery>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let auth_user = get_authenticated_user(&headers, &state.repo);

    let jobs = if query.my == Some(true) {
        if let Some(u) = auth_user {
            state.repo.get_user_jobs(u.id)
        } else {
            return Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Unauthorized"}))));
        }
    } else if let Some(ref j) = query.journalist {
        state.repo.get_jobs_by_journalist(j)
    } else {
        state.repo.get_all_jobs()
    }
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    Ok(Json(serde_json::json!({ "jobs": jobs })))
}

#[derive(Deserialize)]
pub struct CreateJobPayload {
    url: String,
    keyword: Option<String>,
    notes: Option<String>,
    priority: Option<i32>,
    journalist: Option<String>,
    index_str: Option<String>,
}

pub async fn api_create_job(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<CreateJobPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let auth_user = get_authenticated_user(&headers, &state.repo);

    let journalist = if let Some(j) = payload.journalist {
        j.to_uppercase()
    } else if let Some(ref u) = auth_user {
        u.journalist_surname.clone().unwrap_or_else(|| "MCR".into()).to_uppercase()
    } else {
        "MCR".into()
    };

    let keyword = payload
        .keyword
        .unwrap_or_else(|| "ASSET".into())
        .trim()
        .to_uppercase();

    let index_str = payload
        .index_str
        .unwrap_or_else(|| "1".into())
        .trim()
        .to_uppercase();

    let slug = format!("{}_{}_{}", index_str, journalist, keyword);
    let priority = payload.priority.unwrap_or(0);
    let submitted_by = auth_user.map(|u| u.id);

    let job_id = state
        .repo
        .add_job(
            &payload.url,
            &slug,
            &journalist,
            &keyword,
            &index_str,
            priority,
            JobStatus::Pending,
            submitted_by,
            payload.notes.as_deref(),
            None,
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e.to_string()}))))?;

    state.broadcast_event("job_created");
    Ok(Json(serde_json::json!({ "status": "ok", "job_id": job_id, "slug": slug })))
}

#[derive(Deserialize)]
pub struct OverridePayload {
    url: String,
}

pub async fn api_override_job(
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(payload): Json<OverridePayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state
        .repo
        .retry_job(job_id, Some(&payload.url))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

pub async fn api_retry_job(
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state
        .repo
        .retry_job(job_id, None)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

pub async fn api_discard_job(
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state
        .repo
        .delete_job(job_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    state.broadcast_event("job_deleted");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ==========================================
// Journalists API
// ==========================================

pub async fn api_get_journalists(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let list = state
        .repo
        .list_journalists()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "journalists": list })))
}

#[derive(Deserialize)]
pub struct CreateJournalistPayload {
    surname: String,
    full_name: String,
    emails: Vec<String>,
    priority: Option<i32>,
}

pub async fn api_save_journalist(
    State(state): State<AppState>,
    Json(payload): Json<CreateJournalistPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state
        .repo
        .save_journalist(
            &payload.surname,
            &payload.full_name,
            &payload.emails,
            payload.priority.unwrap_or(0),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

pub async fn api_delete_journalist(
    AxumPath(surname): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state
        .repo
        .delete_journalist(&surname)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ==========================================
// Admin API
// ==========================================

pub async fn api_admin_list_users(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    verify_admin(&headers, &state.repo)?;
    let users = state
        .repo
        .list_users()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "users": users })))
}

#[derive(Deserialize)]
pub struct CreateUserPayload {
    email: String,
    full_name: String,
    password: String,
    role: String,
    journalist_surname: Option<String>,
}

pub async fn api_admin_create_user(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<CreateUserPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    verify_admin(&headers, &state.repo)?;
    let role = UserRole::from_str_lossy(&payload.role);
    let id = state
        .repo
        .create_user(
            &payload.email,
            &payload.password,
            role,
            &payload.full_name,
            payload.journalist_surname.as_deref(),
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "status": "ok", "user_id": id })))
}

#[derive(Deserialize)]
pub struct UpdatePasswordPayload {
    password: String,
}

pub async fn api_admin_update_password(
    headers: HeaderMap,
    AxumPath(user_id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(payload): Json<UpdatePasswordPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    verify_admin(&headers, &state.repo)?;
    state
        .repo
        .update_user_password(user_id, &payload.password)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

#[derive(Deserialize)]
pub struct PurgePayload {
    older_than_days: i64,
}

pub async fn api_admin_purge(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<PurgePayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    verify_admin(&headers, &state.repo)?;
    let count = state
        .repo
        .purge_completed_jobs(payload.older_than_days)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "status": "ok", "purged_count": count })))
}

pub async fn api_admin_vacuum(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    verify_admin(&headers, &state.repo)?;
    state
        .repo
        .vacuum_database()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

pub async fn api_admin_dependencies(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    verify_admin(&headers, &state.repo)?;
    let cfg = state.config.read().await;
    let dep_mgr = DependencyManager::new(&cfg.bin_dir);
    let scan = dep_mgr.scan();
    let (update_avail, remote_ver, local_ver) = dep_mgr
        .check_ytdl_update(&cfg.ytdl_channel)
        .await
        .unwrap_or((false, "Unknown".into(), "Unknown".into()));

    Ok(Json(serde_json::json!({
        "dependencies": scan,
        "ytdl_update_available": update_avail,
        "ytdl_remote": remote_ver,
        "ytdl_installed": local_ver
    })))
}

pub async fn api_admin_update_ytdl(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    verify_admin(&headers, &state.repo)?;
    let channel = { state.config.read().await.ytdl_channel.clone() };
    let bin_dir = { state.config.read().await.bin_dir.clone() };
    let dep_mgr = DependencyManager::new(&bin_dir);
    let path = dep_mgr
        .download_ytdl(&channel)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "status": "ok", "path": path.to_string_lossy() })))
}

// ==========================================
// System Status & Setup API
// ==========================================

pub async fn api_system_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (free_gb, total_gb) = {
        let cfg = state.config.read().await;
        let p = std::path::Path::new(&cfg.watchfolder_path);
        match sys_info_disk(p) {
            Some((free, total)) => (free, total),
            None => (0.0, 0.0),
        }
    };

    Json(serde_json::json!({
        "mail_status": "Active",
        "llm_status": "Ready",
        "free_disk_gb": free_gb,
        "total_disk_gb": total_gb,
    }))
}

pub async fn api_system_logs(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let logs = state
        .repo
        .get_recent_audit_logs(50)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    Ok(Json(serde_json::json!({ "logs": logs })))
}

#[derive(Deserialize)]
pub struct TestEmailPayload {
    server: String,
    port: u16,
    email: String,
    pass: String,
}

pub async fn api_test_email(Json(payload): Json<TestEmailPayload>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let res: Result<anyhow::Result<()>, tokio::task::JoinError> = tokio::task::spawn_blocking(move || {
        omni_email::watcher::EmailWatcher::test_connection(&payload.server, payload.port, &payload.email, &payload.pass)
    })
    .await;

    match res {
        Ok(Ok(_)) => Ok(Json(serde_json::json!({"status": "ok"}))),
        Ok(Err(e)) => Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e.to_string()})))),
        Err(join_err) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": join_err.to_string()})))),
    }
}

#[derive(Deserialize)]
pub struct TestLlmPayload {
    endpoint: String,
    model: String,
}

pub async fn api_test_llm(Json(payload): Json<TestLlmPayload>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let client = omni_email::llm::LlmClient::new(&payload.endpoint, &payload.model);
    if client.ping().await {
        Ok(Json(serde_json::json!({"status": "ok"})))
    } else {
        Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "LLM endpoint unreachable"}))))
    }
}

#[derive(Deserialize)]
pub struct SetupPayload {
    email_provider: String,
    imap_server: String,
    email_address: String,
    email_password: String,
    ollama_endpoint: String,
    ollama_model: String,
    watchfolder_path: String,
}

pub async fn api_setup(
    State(state): State<AppState>,
    Json(payload): Json<SetupPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    {
        let mut cfg = state.config.write().await;
        cfg.email_provider = payload.email_provider;
        cfg.imap_server = payload.imap_server;
        cfg.email_address = payload.email_address;
        cfg.email_password = payload.email_password;
        cfg.ollama_endpoint = payload.ollama_endpoint;
        cfg.ollama_model = payload.ollama_model;
        cfg.watchfolder_path = payload.watchfolder_path;
        cfg.save_to_file(&state.config_path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;
    }
    Ok(Json(serde_json::json!({"status": "ok"})))
}

// ==========================================
// Real-Time Server-Sent Events (SSE)
// ==========================================

pub async fn api_events(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| {
        msg.ok().map(|txt| Ok(Event::default().data(txt)))
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
}

// ==========================================
// Helpers
// ==========================================

fn verify_admin(headers: &HeaderMap, repo: &omni_core::repository::Repository) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Some(user) = get_authenticated_user(headers, repo) {
        if user.role == UserRole::Admin {
            return Ok(());
        }
    }
    Err((StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Admin access required"}))))
}

fn sys_info_disk(path: &std::path::Path) -> Option<(f64, f64)> {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        let mut root = path.to_path_buf();
        while root.parent().is_some() && root.parent().unwrap() != std::path::Path::new("") {
            root = root.parent().unwrap().to_path_buf();
        }
        let wide: Vec<u16> = OsStr::new(&root).encode_wide().chain(std::iter::once(0)).collect();

        unsafe {
            let mut free_bytes: u64 = 0;
            let mut total_bytes: u64 = 0;
            let mut total_free_bytes: u64 = 0;

            extern "system" {
                fn GetDiskFreeSpaceExW(
                    lpDirectoryName: *const u16,
                    lpFreeBytesAvailableToCaller: *mut u64,
                    lpTotalNumberOfBytes: *mut u64,
                    lpTotalNumberOfFreeBytes: *mut u64,
                ) -> i32;
            }

            if GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_bytes, &mut total_bytes, &mut total_free_bytes) != 0 {
                let free_gb = (free_bytes as f64) / (1024.0 * 1024.0 * 1024.0);
                let total_gb = (total_bytes as f64) / (1024.0 * 1024.0 * 1024.0);
                return Some((free_gb, total_gb));
            }
        }
    }
    None
}
