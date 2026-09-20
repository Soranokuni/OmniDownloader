use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{sse::Event, IntoResponse, Redirect, Response, Sse};
use axum::Json;
use chrono::Duration as ChronoDuration;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::IpAddr;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use omni_core::auth::{generate_session_token, verify_password};
use omni_core::dependencies::DependencyManager;
use omni_core::models::{JobStatus, User, UserRole};

use crate::assets::EmbeddedFile;
use crate::auth::{
    build_logout_cookie, build_session_cookie, client_ip, extract_session_token,
    internal_error, is_loopback_client, resolve_principal, ApiError, MaybePrincipal, Principal,
    RequireAdmin, RequireMcr, RequireUser,
};
use crate::ratelimit::Decision;
use crate::state::AppState;

type ApiResult<T> = Result<T, ApiError>;
type JsonResult = ApiResult<Json<serde_json::Value>>;

// ==========================================
// Web Page Views
// ==========================================
//
// Pages redirect to /login rather than returning 401, because the visitor is a
// human with a browser. The APIs behind them return the status code; the two
// must not be confused, or the panel shows a JSON blob where a login form
// belongs.

pub async fn handle_root(State(state): State<AppState>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    match resolve_principal(&parts, &state).await {
        Some(Principal::User(u)) => match u.role {
            UserRole::Admin => Redirect::temporary("/admin").into_response(),
            UserRole::OpenMcr => Redirect::temporary("/mcr").into_response(),
            UserRole::User => Redirect::temporary("/user").into_response(),
        },
        Some(Principal::OpenMcr(_)) => Redirect::temporary("/mcr").into_response(),
        None => Redirect::temporary("/login").into_response(),
    }
}

pub async fn view_login() -> impl IntoResponse {
    EmbeddedFile("login.html")
}

pub async fn view_user(principal: Option<RequireUser>) -> Response {
    match principal {
        Some(_) => EmbeddedFile("user.html").into_response(),
        None => Redirect::temporary("/login").into_response(),
    }
}

pub async fn view_mcr(principal: MaybePrincipal) -> Response {
    match principal.0 {
        Some(p) if p.is_mcr() => EmbeddedFile("mcr.html").into_response(),
        _ => Redirect::temporary("/login").into_response(),
    }
}

pub async fn view_admin(principal: Option<RequireAdmin>) -> Response {
    match principal {
        Some(_) => EmbeddedFile("admin.html").into_response(),
        None => Redirect::temporary("/login").into_response(),
    }
}

/// The first-run page. Served only while no administrator exists, and only to a
/// client on this machine (plan P2.4).
///
/// Once an admin exists the route is a 404, not a redirect: a redirect would
/// confirm the page had once been there, and there is nothing at that address
/// any more.
pub async fn view_setup(State(state): State<AppState>, req: Request) -> Response {
    let (parts, _) = req.into_parts();

    if setup_window_open(&parts, &state).await {
        EmbeddedFile("setup.html").into_response()
    } else {
        (StatusCode::NOT_FOUND, "404 Not Found").into_response()
    }
}

/// True while first-run configuration is permitted.
async fn setup_window_open(parts: &axum::http::request::Parts, state: &AppState) -> bool {
    // An existing admin may always reconfigure, from anywhere they can log in.
    if let Some(p) = resolve_principal(parts, state).await {
        if p.is_admin() {
            return true;
        }
    }
    // Otherwise: only before the first admin exists, and only from this box.
    let no_admin = !state.repo.has_active_admin().unwrap_or(true);
    no_admin && is_loopback_client(parts, state).await
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

pub async fn api_login(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return ApiError::bad_request("Malformed request body.").into_response(),
    };
    let payload: LoginPayload = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(_) => return ApiError::bad_request("Malformed request body.").into_response(),
    };

    let (trusted, trust_header) = {
        let cfg = state.config.read().await;
        (
            cfg.security.trusted_proxy_networks(),
            cfg.security.trust_proxy_header,
        )
    };
    let ip = client_ip(&parts, &trusted, trust_header);
    let ip_str = ip.map(|i| i.to_string());
    let user_agent = parts
        .headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Rate limit first, so a locked-out caller never reaches the password
    // comparison and cannot use its timing as an oracle.
    if let Decision::Deny { retry_after } = state.login_limiter.check(ip, &payload.email) {
        let _ = state.repo.record_login_attempt(
            Some(&payload.email),
            ip_str.as_deref(),
            false,
            Some("rate_limited"),
        );
        tracing::warn!(
            ip = ip_str.as_deref().unwrap_or("unknown"),
            "Login rate limit hit"
        );
        let mut response = ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "Too many failed sign-in attempts. Try again shortly.",
        )
        .into_response();
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_str(&retry_after.as_secs().max(1).to_string())
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("60")),
        );
        return response;
    }

    let lookup = state.repo.get_user_by_email(&payload.email);
    let user = match lookup {
        Ok(Some(u)) => u,
        Ok(None) => {
            // Same response and same wording as a wrong password: the login
            // form must not tell an anonymous caller which addresses exist.
            state.login_limiter.record_failure(ip, &payload.email);
            let _ = state.repo.record_login_attempt(
                Some(&payload.email),
                ip_str.as_deref(),
                false,
                Some("unknown_account"),
            );
            return invalid_credentials();
        }
        Err(e) => {
            tracing::error!(error = ?e, "Login lookup failed");
            return ApiError::internal("Sign-in is temporarily unavailable.").into_response();
        }
    };

    if !user.is_active {
        state.login_limiter.record_failure(ip, &payload.email);
        let _ = state.repo.record_login_attempt(
            Some(&payload.email),
            ip_str.as_deref(),
            false,
            Some("inactive_account"),
        );
        return invalid_credentials();
    }

    if !verify_password(&payload.password, &user.password_hash) {
        state.login_limiter.record_failure(ip, &payload.email);
        let _ = state.repo.record_login_attempt(
            Some(&payload.email),
            ip_str.as_deref(),
            false,
            Some("bad_password"),
        );
        tracing::warn!(
            email = %payload.email,
            ip = ip_str.as_deref().unwrap_or("unknown"),
            "Failed login"
        );
        return invalid_credentials();
    }

    let lifetime = session_lifetime(&state, user.role).await;
    let token = generate_session_token();
    if let Err(e) = state.repo.create_session_with_meta(
        user.id,
        &token,
        lifetime,
        ip_str.as_deref(),
        user_agent.as_deref(),
    ) {
        tracing::error!(error = ?e, "Failed creating session");
        return ApiError::internal("Sign-in is temporarily unavailable.").into_response();
    }

    state.login_limiter.record_success(ip, &payload.email);
    let _ = state
        .repo
        .record_login_attempt(Some(&payload.email), ip_str.as_deref(), true, None);
    let _ = state.repo.log_audit(
        "INFO",
        "AUTH",
        &format!(
            "Signed in: {} from {}",
            user.email,
            ip_str.as_deref().unwrap_or("unknown")
        ),
    );

    let cookie = build_session_cookie(&token, lifetime.num_seconds(), state.tls_enabled);
    let role = user.role.as_str().to_string();
    let mut response = Json(LoginResponse {
        status: "ok".into(),
        role,
        user,
    })
    .into_response();
    response.headers_mut().insert(SET_COOKIE, cookie);
    response
}

fn invalid_credentials() -> Response {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "INVALID_CREDENTIALS",
        "Invalid email or password.",
    )
    .into_response()
}

/// Absolute session lifetime for a role (plan P2.2).
async fn session_lifetime(state: &AppState, role: UserRole) -> ChronoDuration {
    let cfg = state.config.read().await;
    match role {
        UserRole::Admin => ChronoDuration::hours(cfg.security.session_hours_admin.max(1)),
        UserRole::OpenMcr => ChronoDuration::days(cfg.security.session_days_mcr.max(1)),
        UserRole::User => ChronoDuration::hours(cfg.security.session_hours_user.max(1)),
    }
}

pub async fn api_logout(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Some(token) = extract_session_token(&headers) {
        let _ = state.repo.delete_session(&token);
    }
    let mut response = Json(serde_json::json!({"status": "logged_out"})).into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, build_logout_cookie(state.tls_enabled));
    response
}

/// End every session for the caller's account, on every device.
pub async fn api_logout_all(RequireUser(user): RequireUser, State(state): State<AppState>) -> Response {
    let ended = state.repo.delete_sessions_for_user(user.id).unwrap_or(0);
    let _ = state.repo.log_audit(
        "INFO",
        "AUTH",
        &format!("{} ended all sessions ({})", user.email, ended),
    );
    let mut response =
        Json(serde_json::json!({"status": "ok", "sessions_ended": ended})).into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, build_logout_cookie(state.tls_enabled));
    response
}

pub async fn api_me(RequireUser(user): RequireUser) -> Json<User> {
    Json(user)
}

#[derive(Deserialize)]
pub struct ChangePasswordPayload {
    current_password: String,
    new_password: String,
}

/// Change one's own password. Requires the current password, so a borrowed
/// unlocked workstation cannot be turned into a permanent account takeover.
pub async fn api_change_password(
    RequireUser(user): RequireUser,
    State(state): State<AppState>,
    Json(payload): Json<ChangePasswordPayload>,
) -> Response {
    if !verify_password(&payload.current_password, &user.password_hash) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "INVALID_CREDENTIALS",
            "Current password is incorrect.",
        )
        .into_response();
    }
    if let Err(e) = validate_password(&payload.new_password) {
        return ApiError::bad_request(e).into_response();
    }
    // This also ends every session for the account, including this one, so the
    // panel sends the operator back to the login screen afterwards.
    match state.repo.update_user_password(user.id, &payload.new_password) {
        Ok(()) => {
            let mut response =
                Json(serde_json::json!({"status": "ok", "reauth_required": true})).into_response();
            response
                .headers_mut()
                .insert(SET_COOKIE, build_logout_cookie(state.tls_enabled));
            response
        }
        Err(e) => {
            tracing::error!(error = ?e, "Password change failed");
            ApiError::internal("Could not change the password.").into_response()
        }
    }
}

/// Minimum password policy.
///
/// Length only. Composition rules ("one digit, one symbol") push operators
/// towards `Password1!` and towards writing it on the desk; a 12-character
/// floor is the part that actually helps.
pub fn validate_password(password: &str) -> Result<(), String> {
    if password.chars().count() < 12 {
        return Err("Password must be at least 12 characters.".to_string());
    }
    if password.chars().count() > 256 {
        return Err("Password must be at most 256 characters.".to_string());
    }
    Ok(())
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
    RequireMcr(principal): RequireMcr,
    Query(query): Query<JobsQuery>,
    State(state): State<AppState>,
) -> JsonResult {
    let jobs = if query.my == Some(true) {
        let Some(user) = principal.user() else {
            return Err(ApiError::unauthorized());
        };
        state.repo.get_user_jobs(user.id)
    } else if let Some(ref j) = query.journalist {
        state.repo.get_jobs_by_journalist(j)
    } else {
        state.repo.get_all_jobs()
    }
    .map_err(internal_error("Could not read the job queue."))?;

    Ok(Json(serde_json::json!({ "jobs": jobs })))
}

/// A journalist's own jobs. Separate from `/api/jobs?my=true` so the user panel
/// needs no MCR access at all.
pub async fn api_get_my_jobs(
    RequireUser(user): RequireUser,
    State(state): State<AppState>,
) -> JsonResult {
    let jobs = state
        .repo
        .get_user_jobs(user.id)
        .map_err(internal_error("Could not read your jobs."))?;
    Ok(Json(serde_json::json!({ "jobs": jobs })))
}

pub async fn api_get_job(
    RequireMcr(_): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    let job = state
        .repo
        .get_job(job_id)
        .map_err(internal_error("Could not read the job."))?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(serde_json::json!({ "job": job })))
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
    RequireMcr(principal): RequireMcr,
    State(state): State<AppState>,
    Json(payload): Json<CreateJobPayload>,
) -> JsonResult {
    let url = payload.url.trim();
    if !is_submittable_url(url) {
        return Err(ApiError::bad_request(
            "Enter an http:// or https:// link.",
        ));
    }

    let auth_user = principal.user();

    let journalist = if let Some(j) = payload.journalist.as_deref() {
        sanitize_token(j, "MCR")
    } else if let Some(u) = auth_user {
        sanitize_token(u.journalist_surname.as_deref().unwrap_or("MCR"), "MCR")
    } else {
        "MCR".to_string()
    };

    let keyword = sanitize_token(payload.keyword.as_deref().unwrap_or("ASSET"), "ASSET");
    let index_str = sanitize_token(payload.index_str.as_deref().unwrap_or("1"), "1");

    let slug = format!("{}_{}_{}", index_str, journalist, keyword);
    let priority = payload.priority.unwrap_or(0).clamp(-100, 100);
    let submitted_by = auth_user.map(|u| u.id);

    let job_id = state
        .repo
        .add_job(
            url,
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
        .map_err(|e| {
            tracing::error!(error = ?e, "Enqueue failed");
            ApiError::bad_request("Could not queue that link.")
        })?;

    state.broadcast_event("job_created");
    Ok(Json(
        serde_json::json!({ "status": "ok", "job_id": job_id, "slug": slug }),
    ))
}

/// Accept only what the pipeline can actually be handed.
///
/// `javascript:` and `data:` links are the interesting ones: they are harmless
/// to yt-dlp but become live code the moment a panel renders one as an `href`.
/// Rejecting them at the door is cheaper than trusting every future renderer.
fn is_submittable_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && url.len() <= 2048
        && !url.chars().any(|c| c.is_control())
}

/// Reduce an operator-supplied token to what may appear in a filename.
///
/// These three fields become the MXF's name in the playout watchfolder, so they
/// are constrained to `[A-Z0-9-]` — not as an XSS defence (the panels escape
/// separately) but because a slug is a filename and a filename with a quote,
/// a slash or a NUL in it is a delivery failure at best.
fn sanitize_token(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .trim()
        .to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(40)
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

#[derive(Deserialize)]
pub struct OverridePayload {
    url: String,
}

pub async fn api_override_job(
    RequireMcr(principal): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(payload): Json<OverridePayload>,
) -> JsonResult {
    if !is_submittable_url(&payload.url) {
        return Err(ApiError::bad_request("Enter an http:// or https:// link."));
    }
    state
        .repo
        .retry_job(job_id, Some(payload.url.trim()))
        .map_err(internal_error("Could not update the job."))?;

    audit_action(&state, &principal, &format!("Job #{job_id}: URL overridden"));
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

pub async fn api_retry_job(
    RequireMcr(principal): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    state
        .repo
        .retry_job(job_id, None)
        .map_err(internal_error("Could not retry the job."))?;

    audit_action(&state, &principal, &format!("Job #{job_id}: retried"));
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

pub async fn api_discard_job(
    RequireMcr(principal): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    state
        .repo
        .delete_job(job_id)
        .map_err(internal_error("Could not discard the job."))?;

    audit_action(&state, &principal, &format!("Job #{job_id}: discarded"));
    state.broadcast_event("job_deleted");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// Record who did a destructive thing.
///
/// An allowlisted client logs as `open-mcr <ip>` — less than a name, but it is
/// the truth about what we know, and "unknown" in an audit row is worse than a
/// narrow fact.
fn audit_action(state: &AppState, principal: &Principal, message: &str) {
    let _ = state.repo.log_audit(
        "INFO",
        "MCR",
        &format!("{} — by {}", message, principal.audit_label()),
    );
}

// ==========================================
// Journalists API
// ==========================================

pub async fn api_get_journalists(
    RequireMcr(_): RequireMcr,
    State(state): State<AppState>,
) -> JsonResult {
    let list = state
        .repo
        .list_journalists()
        .map_err(internal_error("Could not read the journalist roster."))?;
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
    RequireMcr(principal): RequireMcr,
    State(state): State<AppState>,
    Json(payload): Json<CreateJournalistPayload>,
) -> JsonResult {
    let surname = sanitize_token(&payload.surname, "");
    if surname.is_empty() {
        return Err(ApiError::bad_request("Surname is required."));
    }
    if payload.emails.len() > 20 {
        return Err(ApiError::bad_request("At most 20 addresses per journalist."));
    }
    state
        .repo
        .save_journalist(
            &surname,
            payload.full_name.trim(),
            &payload.emails,
            payload.priority.unwrap_or(0).clamp(-100, 100),
        )
        .map_err(internal_error("Could not save the journalist."))?;
    audit_action(&state, &principal, &format!("Journalist {surname} saved"));
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

pub async fn api_delete_journalist(
    RequireMcr(principal): RequireMcr,
    AxumPath(surname): AxumPath<String>,
    State(state): State<AppState>,
) -> JsonResult {
    if surname.trim().eq_ignore_ascii_case("MCR") {
        // MCR is structural: the parser assigns it to everything it cannot
        // resolve and delivery uses it as a folder name.
        return Err(ApiError::bad_request(
            "MCR is the fallback destination and cannot be removed.",
        ));
    }
    state
        .repo
        .delete_journalist(&surname)
        .map_err(internal_error("Could not delete the journalist."))?;
    audit_action(&state, &principal, &format!("Journalist {surname} deleted"));
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ==========================================
// Admin API
// ==========================================

pub async fn api_admin_list_users(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
) -> JsonResult {
    let users = state
        .repo
        .list_users()
        .map_err(internal_error("Could not read the user list."))?;
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
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<CreateUserPayload>,
) -> JsonResult {
    validate_password(&payload.password).map_err(ApiError::bad_request)?;
    if !payload.email.contains('@') {
        return Err(ApiError::bad_request("A valid email address is required."));
    }
    let role = UserRole::from_str_lossy(&payload.role);
    let id = state
        .repo
        .create_user(
            payload.email.trim(),
            &payload.password,
            role,
            payload.full_name.trim(),
            payload.journalist_surname.as_deref(),
        )
        .map_err(|e| {
            tracing::error!(error = ?e, "User creation failed");
            ApiError::bad_request("Could not create the account (is the address already in use?).")
        })?;
    let _ = state.repo.log_audit(
        "INFO",
        "ADMIN",
        &format!(
            "User {} created with role {} by {}",
            payload.email.trim(),
            role.as_str(),
            admin.email
        ),
    );
    Ok(Json(serde_json::json!({ "status": "ok", "user_id": id })))
}

#[derive(Deserialize)]
pub struct UpdatePasswordPayload {
    password: String,
}

pub async fn api_admin_update_password(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(user_id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(payload): Json<UpdatePasswordPayload>,
) -> JsonResult {
    validate_password(&payload.password).map_err(ApiError::bad_request)?;
    state
        .repo
        .update_user_password(user_id, &payload.password)
        .map_err(internal_error("Could not update the password."))?;
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!("Password for user #{user_id} reset by {}", admin.email),
    );
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

#[derive(Deserialize)]
pub struct PurgePayload {
    older_than_days: i64,
}

pub async fn api_admin_purge(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<PurgePayload>,
) -> JsonResult {
    if payload.older_than_days < 1 {
        return Err(ApiError::bad_request(
            "Choose a retention window of at least one day.",
        ));
    }
    let count = state
        .repo
        .purge_completed_jobs(payload.older_than_days)
        .map_err(internal_error("Could not purge the archive."))?;
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!(
            "{} purged {} completed jobs older than {} days",
            admin.email, count, payload.older_than_days
        ),
    );
    Ok(Json(
        serde_json::json!({ "status": "ok", "purged_count": count }),
    ))
}

pub async fn api_admin_vacuum(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
) -> JsonResult {
    state
        .repo
        .vacuum_database()
        .map_err(internal_error("Could not compact the database."))?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

pub async fn api_admin_dependencies(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
) -> JsonResult {
    let (bin_dir, channel) = {
        let cfg = state.config.read().await;
        (cfg.bin_dir.clone(), cfg.ytdl_channel.clone())
    };
    let dep_mgr = DependencyManager::new(&bin_dir);
    let scan = dep_mgr.scan();
    let (update_avail, remote_ver, local_ver) = dep_mgr
        .check_ytdl_update(&channel)
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
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
) -> JsonResult {
    let (channel, bin_dir) = {
        let cfg = state.config.read().await;
        (cfg.ytdl_channel.clone(), cfg.bin_dir.clone())
    };
    let dep_mgr = DependencyManager::new(&bin_dir);
    let path = dep_mgr
        .download_ytdl(&channel)
        .await
        .map_err(|e| {
            tracing::error!(error = ?e, "yt-dlp update failed");
            ApiError::internal("yt-dlp update failed; see the log for detail.")
        })?;
    let _ = state.repo.log_audit(
        "INFO",
        "ADMIN",
        &format!("{} updated yt-dlp ({} channel)", admin.email, channel),
    );
    Ok(Json(
        serde_json::json!({ "status": "ok", "path": path.to_string_lossy() }),
    ))
}

// ==========================================
// System Status & Setup API
// ==========================================

pub async fn api_health() -> Json<serde_json::Value> {
    // Public, and deliberately says nothing beyond "the process is answering".
    // Anything about disk, mailbox or queue depth belongs behind the MCR gate.
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

pub async fn api_system_status(
    RequireMcr(_): RequireMcr,
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let (free_gb, total_gb) = {
        let cfg = state.config.read().await;
        let p = std::path::Path::new(&cfg.watchfolder_path);
        sys_info_disk(p).unwrap_or((0.0, 0.0))
    };

    Json(serde_json::json!({
        "mail_status": "Active",
        "llm_status": "Ready",
        "free_disk_gb": free_gb,
        "total_disk_gb": total_gb,
    }))
}

pub async fn api_system_logs(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
) -> JsonResult {
    let logs = state
        .repo
        .get_recent_audit_logs(50)
        .map_err(internal_error("Could not read the log."))?;
    Ok(Json(serde_json::json!({ "logs": logs })))
}

#[derive(Deserialize)]
pub struct TestEmailPayload {
    server: String,
    port: u16,
    email: String,
    pass: String,
}

/// Test the mailbox credentials.
///
/// Admin-only: unauthenticated, this was a credential oracle — point it at the
/// station's mailbox and the response distinguishes a right password from a
/// wrong one, for free, from anywhere on the LAN.
pub async fn api_test_email(
    RequireAdmin(_): RequireAdmin,
    Json(payload): Json<TestEmailPayload>,
) -> JsonResult {
    let res: Result<anyhow::Result<()>, tokio::task::JoinError> =
        tokio::task::spawn_blocking(move || {
            omni_email::watcher::EmailWatcher::test_connection(
                &payload.server,
                payload.port,
                &payload.email,
                &payload.pass,
            )
        })
        .await;

    match res {
        Ok(Ok(_)) => Ok(Json(serde_json::json!({"status": "ok"}))),
        Ok(Err(e)) => Err(ApiError::bad_request(e.to_string())),
        Err(join_err) => {
            tracing::error!(error = ?join_err, "Mail test task failed");
            Err(ApiError::internal("Mail test could not be run."))
        }
    }
}

#[derive(Deserialize)]
pub struct TestLlmPayload {
    endpoint: String,
    model: String,
}

pub async fn api_test_llm(
    RequireAdmin(_): RequireAdmin,
    Json(payload): Json<TestLlmPayload>,
) -> JsonResult {
    let client = omni_email::llm::LlmClient::new(&payload.endpoint, &payload.model);
    if client.ping().await {
        Ok(Json(serde_json::json!({"status": "ok"})))
    } else {
        Err(ApiError::bad_request("LLM endpoint unreachable"))
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
    /// Only honoured while no administrator exists.
    admin_email: Option<String>,
    admin_password: Option<String>,
    admin_full_name: Option<String>,
}

/// First-run configuration (plan P2.4).
///
/// Unauthenticated, this route let anyone on the newsroom LAN repoint
/// `watchfolder_path`. Every hardening in Phases 0–1 then guaranteed a correct,
/// atomically delivered MXF — into the attacker's directory.
pub async fn api_setup(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if !setup_window_open(&parts, &state).await {
        // 404, matching the page: there is nothing here for this caller.
        return ApiError::not_found().into_response();
    }

    let bytes = match axum::body::to_bytes(body, 256 * 1024).await {
        Ok(b) => b,
        Err(_) => return ApiError::bad_request("Malformed request body.").into_response(),
    };
    let payload: SetupPayload = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(_) => return ApiError::bad_request("Malformed request body.").into_response(),
    };

    // Create the first administrator, if this is that moment.
    let needs_admin = !state.repo.has_active_admin().unwrap_or(true);
    if needs_admin {
        let (Some(email), Some(password)) =
            (payload.admin_email.as_deref(), payload.admin_password.as_deref())
        else {
            return ApiError::bad_request(
                "Create the administrator account to finish setup.",
            )
            .into_response();
        };
        if let Err(e) = validate_password(password) {
            return ApiError::bad_request(e).into_response();
        }
        if !email.contains('@') {
            return ApiError::bad_request("A valid email address is required.").into_response();
        }
        if let Err(e) = state.repo.create_user(
            email.trim(),
            password,
            UserRole::Admin,
            payload
                .admin_full_name
                .as_deref()
                .unwrap_or("System Administrator")
                .trim(),
            None,
        ) {
            tracing::error!(error = ?e, "First-run admin creation failed");
            return ApiError::bad_request("Could not create the administrator account.")
                .into_response();
        }
        let _ = state.repo.log_audit(
            "WARN",
            "ADMIN",
            &format!("First-run administrator {} created", email.trim()),
        );
    }

    {
        let mut cfg = state.config.write().await;
        cfg.email_provider = payload.email_provider;
        cfg.imap_server = payload.imap_server;
        cfg.email_address = payload.email_address;
        if !payload.email_password.is_empty() {
            cfg.email_password = payload.email_password;
        }
        cfg.ollama_endpoint = payload.ollama_endpoint;
        cfg.ollama_model = payload.ollama_model;
        cfg.watchfolder_path = payload.watchfolder_path;
        if let Err(e) = cfg.save_to_file(&state.config_path) {
            tracing::error!(error = ?e, "Saving config failed");
            return ApiError::internal("Could not save the configuration.").into_response();
        }
    }
    let _ = state
        .repo
        .log_audit("WARN", "ADMIN", "Configuration changed via /setup");

    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Whether the first-run page should still be offered in the UI.
/// Public, because the login page links to it and must know.
pub async fn api_setup_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "needs_admin": !state.repo.has_active_admin().unwrap_or(true)
    }))
}

// ==========================================
// Real-Time Server-Sent Events (SSE)
// ==========================================

pub async fn api_events(
    RequireMcr(_): RequireMcr,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream =
        BroadcastStream::new(rx).filter_map(|msg| msg.ok().map(|txt| Ok(Event::default().data(txt))));
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
}

// ==========================================
// Helpers
// ==========================================

/// Free and total gigabytes on the volume holding `path`.
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
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    None
}

/// Exposed for tests that need the same address parsing the extractors use.
#[doc(hidden)]
pub fn parse_ip(s: &str) -> Option<IpAddr> {
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_tokens_are_reduced_to_filename_safe_text() {
        assert_eq!(sanitize_token("papadaki", "MCR"), "PAPADAKI");
        assert_eq!(sanitize_token("  1a  ", "1"), "1A");
        // A slug becomes a filename in the playout watchfolder: separators and
        // quotes must never survive into it.
        assert_eq!(sanitize_token("../../etc", "MCR"), "ETC");
        assert_eq!(sanitize_token("a\"b", "MCR"), "A-B");
        assert_eq!(sanitize_token("", "MCR"), "MCR");
        assert_eq!(sanitize_token("---", "MCR"), "MCR");
        assert_eq!(sanitize_token(&"X".repeat(200), "MCR").len(), 40);
    }

    #[test]
    fn only_http_urls_are_submittable() {
        assert!(is_submittable_url("https://www.youtube.com/watch?v=abc"));
        assert!(is_submittable_url("http://neakriti.gr/video/1"));
        assert!(!is_submittable_url("javascript:alert(1)"));
        assert!(!is_submittable_url("data:text/html,<script>"));
        assert!(!is_submittable_url("file:///C:/Windows/System32"));
        assert!(!is_submittable_url(""));
        assert!(!is_submittable_url(&format!("https://x/{}", "a".repeat(3000))));
        assert!(!is_submittable_url("https://x/\u{0}evil"));
    }

    #[test]
    fn password_policy_is_length_based() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("correct-horse-battery").is_ok());
        assert!(validate_password(&"x".repeat(300)).is_err());
    }
}
