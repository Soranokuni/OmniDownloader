use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{sse::Event, IntoResponse, Redirect, Response, Sse};
use axum::Json;
use chrono::Duration as ChronoDuration;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
            "Πάρα πολλές αποτυχημένες προσπάθειες σύνδεσης. Δοκιμάστε ξανά σε λίγο.",
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
            return ApiError::internal("Η σύνδεση δεν είναι διαθέσιμη αυτή τη στιγμή.").into_response();
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
        "Λάθος email ή κωδικός.",
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
            omni_core::selftest::check_default_admin(&state.repo, &state.health);
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

/// Minimum password policy: one rule for the panels and the CLI.
pub use omni_core::auth::validate_password;

// ==========================================
// Jobs API
// ==========================================

#[derive(Deserialize)]
pub struct JobsQuery {
    my: Option<bool>,
    journalist: Option<String>,
    /// `live`, `review` or `completed`: one page of an MCR desk list
    /// (plan P7.1). Without it, the old unpaged list.
    view: Option<omni_core::models::JobsView>,
    page: Option<i64>,
    per_page: Option<i64>,
    q: Option<String>,
    group: Option<String>,
}

/// A job as the MCR desk shows it: the row, plus what its error code means
/// and what to do about it, in the operators' language.
fn job_for_desk(job: &omni_core::models::Job) -> serde_json::Value {
    let mut v = serde_json::to_value(job).unwrap_or_default();
    if let Some(code) = job.error_code.as_deref().and_then(omni_broadcast::errors::ErrorCode::from_code) {
        v["hint"] = serde_json::Value::String(code.hint_el().to_string());
    }
    v
}

pub async fn api_get_jobs(
    RequireMcr(principal): RequireMcr,
    Query(query): Query<JobsQuery>,
    State(state): State<AppState>,
) -> JsonResult {
    if let Some(view) = query.view {
        let filter = omni_core::models::JobsFilter {
            search: query.q.clone().unwrap_or_default(),
            journalist: query.journalist.clone().unwrap_or_default(),
            group: query.group.clone().unwrap_or_default(),
        };
        let page = state
            .repo
            .list_jobs_page(view, &filter, query.page.unwrap_or(1), query.per_page.unwrap_or(20))
            .map_err(internal_error("Could not read the job queue."))?;
        let counts = state.repo.job_counts().map_err(internal_error("Could not count the jobs."))?;
        // Which mail each came from, so the desk can open it (plan P7.11).
        let keys: Vec<String> = page.jobs.iter().filter_map(|j| j.email_message_id.clone()).collect();
        let subjects = state.repo.mail_subjects(&keys).unwrap_or_default();
        let jobs: Vec<serde_json::Value> = page
            .jobs
            .iter()
            .map(|j| {
                let mut v = job_for_desk(j);
                if let Some(subject) = j.email_message_id.as_ref().and_then(|k| subjects.get(k)) {
                    v["mail_subject"] = serde_json::Value::String(subject.clone());
                }
                v
            })
            .collect();
        return Ok(Json(serde_json::json!({
            "jobs": jobs,
            "total": page.total,
            "page": page.page,
            "per_page": page.per_page,
            "counts": counts,
        })));
    }
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
    // The timeline the mail view shows for a selected video (plan P7.8).
    let events = state
        .repo
        .recent_job_events(job_id, 80)
        .map_err(internal_error("Could not read the job."))?;
    Ok(Json(serde_json::json!({ "job": job_for_desk(&job), "events": events })))
}

#[derive(Deserialize)]
pub struct CreateJobPayload {
    url: String,
    keyword: Option<String>,
    notes: Option<String>,
    priority: Option<i32>,
    journalist: Option<String>,
    index_str: Option<String>,
    /// Only the first N videos of the article (plan P4.33).
    max_videos: Option<i64>,
}

pub async fn api_create_job(
    RequireMcr(principal): RequireMcr,
    State(state): State<AppState>,
    Json(payload): Json<CreateJobPayload>,
) -> JsonResult {
    let url = payload.url.trim();
    if !is_submittable_url(url) {
        return Err(ApiError::bad_request(
            "Επικολλήστε έναν σύνδεσμο που αρχίζει με http:// ή https://.",
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

    let mut new = omni_core::models::NewJob::new(url, slug.clone(), journalist.clone());
    new.keyword = keyword.clone();
    new.index_str = index_str.clone();
    new.priority = priority;
    new.status = JobStatus::Pending;
    new.submitted_by_user_id = submitted_by;
    new.notes = payload.notes.clone();
    new.max_videos = payload.max_videos.filter(|n| (1..=20).contains(n));
    let job_id = state
        .repo
        .enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
        .map_err(|e| {
            tracing::error!(error = ?e, "Enqueue failed");
            ApiError::bad_request("Ο σύνδεσμος δεν μπήκε στην ουρά.")
        })?
        .job_id();

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
    // Greek typed in the form ("Σεισμός") becomes the house Latin form
    // (ELOT 743, as the email parser makes keywords), not a row of dashes.
    let latin = omni_core::translit::translit(raw.trim());
    let cleaned: String = latin
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
        return Err(ApiError::bad_request("Επικολλήστε έναν σύνδεσμο που αρχίζει με http:// ή https://."));
    }
    let retried = state
        .repo
        .retry_job(job_id, Some(payload.url.trim()))
        .map_err(internal_error("Could not update the job."))?;
    if !retried {
        return Err(not_retryable());
    }

    audit_action(&state, &principal, &format!("Job #{job_id}: URL overridden"));
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

#[derive(Deserialize)]
pub struct RenamePayload {
    keyword: String,
}

/// MCR renames a video before it is made (P7.12): the keyword part of the
/// file name, typed in Greek or Latin, kept in the house form (ELOT 743,
/// letters and digits, at most 20).
pub async fn api_rename_job(
    RequireMcr(principal): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(payload): Json<RenamePayload>,
) -> JsonResult {
    let keyword = house_keyword(&payload.keyword)
        .ok_or_else(|| ApiError::bad_request("Γράψτε μια λέξη-κλειδί με τουλάχιστον 2 γράμματα ή ψηφία."))?;
    let renamed = state
        .repo
        .rename_job_keyword(job_id, &keyword)
        .map_err(internal_error("Could not rename the job."))?;
    let Some((old, new)) = renamed else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "TOO_LATE",
            "Το αρχείο αυτού του βίντεο δημιουργείται ή έχει ήδη παραδοθεί· το όνομά του δεν αλλάζει πια.",
        ));
    };
    let who = principal.audit_label();
    let _ = state
        .repo
        .record_event(job_id, "INFO", None, &format!("Renamed by {who}: {old} → {new}"));
    audit_action(&state, &principal, &format!("Job #{job_id}: renamed {old} → {new}"));
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok", "slug": new, "keyword": keyword })))
}

/// A keyword as typed by a person: transliterated, letters and digits only,
/// upper case, at most 20; `None` under 2 characters.
fn house_keyword(raw: &str) -> Option<String> {
    let k: String = omni_core::translit::translit(raw.trim())
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(20)
        .collect();
    (k.len() >= 2).then_some(k)
}

#[derive(Deserialize)]
pub struct QueueOfferPayload {
    url: String,
}

/// Queue one of the videos the sniffer found in a job's article and offered
/// to MCR (policy C, `omni_broadcast::article`).
///
/// Only a URL that is on the job's offer list is accepted, and only once: the
/// offer records the job it became. Arbitrary URLs go through the normal
/// "new job" form, not here.
pub async fn api_queue_offer(
    RequireMcr(principal): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(payload): Json<QueueOfferPayload>,
) -> JsonResult {
    use omni_broadcast::article::Offered;

    let job = state
        .repo
        .get_job(job_id)
        .map_err(internal_error("Could not read the job."))?
        .ok_or_else(ApiError::not_found)?;
    let mut offers: Vec<Offered> = job
        .candidates_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    let Some(offer) = offers.iter_mut().find(|o| o.url == payload.url.trim()) else {
        return Err(ApiError::bad_request("Αυτό το βίντεο δεν είναι ανάμεσα στα προτεινόμενα αυτής της εργασίας."));
    };
    if let Some(existing) = offer.queued_job_id {
        return Err(ApiError::bad_request(format!("Έχει ήδη μπει στην ουρά ως εργασία #{existing}.")));
    }

    let mut new = omni_core::models::NewJob::new(
        offer.url.clone(),
        format!("{}_{}_{}", offer.index_str, job.journalist, job.keyword),
        job.journalist.clone(),
    );
    new.keyword = job.keyword.clone();
    new.index_str = offer.index_str.clone();
    new.priority = job.priority;
    new.email_source = job.email_source.clone();
    new.email_message_id = job.email_message_id.clone();
    new.submitted_by_user_id = principal.user().map(|u| u.id).or(job.submitted_by_user_id);
    new.extraction_method = Some("sniffer".into());
    new.group_code = job.group_code.clone();
    new.parent_job_id = Some(job_id);
    new.notes = Some(format!("Offered from the article of job #{job_id}: {}", job.url));
    let result = state
        .repo
        .enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
        .map_err(internal_error("Could not queue that video."))?;
    offer.queued_job_id = Some(result.job_id());
    let index = offer.index_str.clone();

    let json = serde_json::to_string(&offers).map_err(|_| ApiError::internal("Could not save the offer."))?;
    state
        .repo
        .set_candidates(job_id, Some(&json))
        .map_err(internal_error("Could not save the offer."))?;
    let _ = state.repo.record_event(
        job_id,
        "INFO",
        None,
        &format!("Offered video queued by MCR as {index} (job #{}): {}", result.job_id(), payload.url.trim()),
    );
    audit_action(&state, &principal, &format!("Job #{job_id}: offered video queued as job #{}", result.job_id()));
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok", "job_id": result.job_id(), "index_str": index })))
}

/// A running or delivered job is not retried (P7.13).
fn not_retryable() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "NOT_RETRYABLE",
        "Αυτό το βίντεο κατεβαίνει αυτή τη στιγμή ή έχει ήδη παραδοθεί. Για νέο αρχείο από παραδομένο βίντεο, πατήστε «Νέα λήψη».",
    )
}

pub async fn api_retry_job(
    RequireMcr(principal): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    let retried = state
        .repo
        .retry_job(job_id, None)
        .map_err(internal_error("Could not retry the job."))?;
    if !retried {
        return Err(not_retryable());
    }

    audit_action(&state, &principal, &format!("Job #{job_id}: retried"));
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// Take every delivered job off the live queue. Nothing is deleted.
pub async fn api_clear_finished(RequireMcr(principal): RequireMcr, State(state): State<AppState>) -> JsonResult {
    let cleared = state
        .repo
        .clear_finished_jobs()
        .map_err(internal_error("Could not clear the finished jobs."))?;
    audit_action(&state, &principal, &format!("Cleared {cleared} finished job(s) from the live queue"));
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok", "cleared": cleared })))
}

/// Download and convert a delivered job again from its link: the file was
/// deleted, or something went wrong with it.
pub async fn api_redownload_job(
    RequireMcr(principal): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    let done = state
        .repo
        .redownload_job(job_id)
        .map_err(internal_error("Could not queue the job again."))?;
    if !done {
        return Err(ApiError::bad_request(
            "Νέα λήψη γίνεται μόνο για βίντεο που έχει παραδοθεί. Για ένα που χρειάζεται έλεγχο, πατήστε «Δοκιμή ξανά».",
        ));
    }
    let _ = state.repo.record_event(
        job_id,
        "INFO",
        None,
        &format!("Download again requested by {}", principal.audit_label()),
    );
    audit_action(&state, &principal, &format!("Job #{job_id}: download again"));
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// MCR put the video into Dalet by hand: the job is recorded as done.
pub async fn api_mark_done_job(
    RequireMcr(principal): RequireMcr,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    let done = state
        .repo
        .mark_completed_manually(job_id)
        .map_err(internal_error("Could not mark the job as done."))?;
    if !done {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "NOT_MARKABLE",
            "Μόνο ένα βίντεο που περιμένει έλεγχο μπορεί να σημειωθεί ως παραδομένο χειροκίνητα.",
        ));
    }
    let _ = state.repo.record_event(
        job_id,
        "INFO",
        None,
        &format!("Finished as COMPLETED_MANUAL: put into Dalet by hand by {}", principal.audit_label()),
    );
    audit_action(&state, &principal, &format!("Job #{job_id}: marked as put into Dalet by hand"));
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
    /// Parser aliases (plan P4.3). Absent leaves the stored ones untouched.
    aliases: Option<Vec<String>>,
}

pub async fn api_save_journalist(
    RequireMcr(principal): RequireMcr,
    State(state): State<AppState>,
    Json(payload): Json<CreateJournalistPayload>,
) -> JsonResult {
    let surname = sanitize_token(&payload.surname, "");
    if surname.is_empty() {
        return Err(ApiError::bad_request("Το επώνυμο είναι υποχρεωτικό."));
    }
    if payload.emails.len() > 20 {
        return Err(ApiError::bad_request("Έως 20 διευθύνσεις ανά δημοσιογράφο."));
    }
    if let Some(aliases) = &payload.aliases {
        if aliases.len() > 30 || aliases.iter().any(|a| a.chars().count() > 40) {
            return Err(ApiError::bad_request("At most 30 aliases of 40 characters each."));
        }
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
    if let Some(aliases) = &payload.aliases {
        state
            .repo
            .set_journalist_aliases(&surname, aliases)
            .map_err(internal_error("Could not save the journalist's aliases."))?;
    }
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
        .backup_taxonomy("before-delete")
        .and_then(|_| state.repo.delete_journalist(&surname))
        .map_err(internal_error("Could not delete the journalist."))?;
    audit_action(&state, &principal, &format!("Journalist {surname} deleted"));
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ==========================================
// Taxonomy: groups and membership (plan P4.20)
// ==========================================

/// The groups, for the MCR label and filter and the admin editor. Group
/// names and descriptions are newsroom vocabulary, not personal data.
pub async fn api_get_groups(RequireMcr(_): RequireMcr, State(state): State<AppState>) -> JsonResult {
    let groups = state
        .repo
        .list_groups()
        .map_err(internal_error("Could not read the groups."))?;
    Ok(Json(serde_json::json!({ "groups": groups })))
}

/// Create or update one group. Admin: groups shape what the LLM is told.
pub async fn api_save_group(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(group): Json<omni_core::taxonomy::Group>,
) -> JsonResult {
    let group = group.normalized();
    group.validate().map_err(ApiError::bad_request)?;
    let saved = state
        .repo
        .save_group(&group)
        .map_err(internal_error("Could not save the group."))?;
    let _ = state
        .repo
        .log_audit("INFO", "ADMIN", &format!("Group {} saved by {}", saved.code, admin.email));
    Ok(Json(serde_json::json!({ "status": "ok", "group": saved })))
}

pub async fn api_delete_group(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(code): AxumPath<String>,
    State(state): State<AppState>,
) -> JsonResult {
    let removed = state
        .repo
        .backup_taxonomy("before-delete")
        .and_then(|_| state.repo.delete_group(&code))
        .map_err(internal_error("Could not delete the group."))?;
    if !removed {
        return Err(ApiError::bad_request("No such group."));
    }
    let _ = state
        .repo
        .log_audit("WARN", "ADMIN", &format!("Group {} deleted by {}", code.to_uppercase(), admin.email));
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

#[derive(Deserialize)]
pub struct JournalistGroupsPayload {
    /// Group codes, the default first.
    groups: Vec<String>,
}

pub async fn api_set_journalist_groups(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(surname): AxumPath<String>,
    State(state): State<AppState>,
    Json(payload): Json<JournalistGroupsPayload>,
) -> JsonResult {
    if payload.groups.len() > 20 {
        return Err(ApiError::bad_request("At most 20 groups per person."));
    }
    let known: Vec<String> = state
        .repo
        .list_groups()
        .map_err(internal_error("Could not read the groups."))?
        .into_iter()
        .map(|g| g.code)
        .collect();
    let codes: Vec<String> = payload.groups.iter().map(|g| g.trim().to_uppercase()).filter(|g| !g.is_empty()).collect();
    if let Some(unknown) = codes.iter().find(|c| !known.contains(c)) {
        return Err(ApiError::bad_request(format!("No group {unknown}.")));
    }
    let surname = surname.trim().to_uppercase();
    if !state
        .repo
        .list_journalists()
        .map_err(internal_error("Could not read the roster."))?
        .iter()
        .any(|j| j.surname == surname)
    {
        return Err(ApiError::bad_request("No such journalist."));
    }
    state
        .repo
        .set_journalist_groups(&surname, &codes)
        .map_err(internal_error("Could not save the groups."))?;
    let _ = state.repo.log_audit(
        "INFO",
        "ADMIN",
        &format!("Groups of {surname} set to [{}] by {}", codes.join(", "), admin.email),
    );
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// Taxonomy backups, newest first (plan P4.27).
pub async fn api_admin_taxonomy_backups(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let backups = state
        .repo
        .list_taxonomy_backups()
        .map_err(internal_error("Could not list the backups."))?;
    Ok(Json(serde_json::json!({
        "backups": backups,
        "kept": omni_core::repository::TAXONOMY_BACKUPS_KEPT,
    })))
}

pub async fn api_admin_taxonomy_backup_now(RequireAdmin(admin): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let b = state
        .repo
        .backup_taxonomy("manual")
        .map_err(internal_error("Could not write the backup."))?;
    let _ = state.repo.log_audit("INFO", "ADMIN", &format!("Taxonomy backed up by {}: {}", admin.email, b.name));
    Ok(Json(serde_json::json!({ "status": "ok", "backup": b })))
}

/// One backup, for download.
pub async fn api_admin_taxonomy_backup_get(
    RequireAdmin(_): RequireAdmin,
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
) -> JsonResult {
    let t = state
        .repo
        .read_taxonomy_backup(&name)
        .map_err(|_| ApiError::bad_request("No such backup."))?;
    Ok(Json(serde_json::json!(t)))
}

pub async fn api_admin_taxonomy_backup_restore(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
) -> JsonResult {
    if state.repo.read_taxonomy_backup(&name).is_err() {
        return Err(ApiError::bad_request("No such backup."));
    }
    let report = state
        .repo
        .restore_taxonomy_backup(&name)
        .map_err(internal_error("Could not restore the backup."))?;
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!("Taxonomy restored from {name} by {} (the state before was backed up)", admin.email),
    );
    Ok(Json(serde_json::json!({ "status": "ok", "report": report })))
}

/// taxonomy.json, for download. Admin only: it lists staff and addresses.
pub async fn api_admin_export_taxonomy(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let t = state
        .repo
        .export_taxonomy()
        .map_err(internal_error("Could not export the taxonomy."))?;
    Ok(Json(serde_json::json!(t)))
}

#[derive(Deserialize)]
pub struct ImportTaxonomyPayload {
    taxonomy: omni_core::taxonomy::Taxonomy,
    /// Also delete groups and people the file does not list.
    #[serde(default)]
    replace: bool,
}

pub async fn api_admin_import_taxonomy(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<ImportTaxonomyPayload>,
) -> JsonResult {
    // Check first, so a mistake in the file comes back as a list the admin
    // can fix, not as "internal error".
    let known: Vec<String> = if payload.replace {
        Vec::new()
    } else {
        state
            .repo
            .list_groups()
            .map_err(internal_error("Could not read the groups."))?
            .into_iter()
            .map(|g| g.code)
            .collect()
    };
    if let Err(problems) = payload.taxonomy.checked(&known) {
        return Err(ApiError::bad_request(format!("The file was not imported:\n- {}", problems.join("\n- "))));
    }
    if payload.taxonomy.people.iter().any(|p| p.surname.trim().eq_ignore_ascii_case("MCR")) {
        return Err(ApiError::bad_request("The file was not imported: MCR is built in."));
    }
    // What was there before, so an import can be undone from the panel.
    state
        .repo
        .backup_taxonomy("before-import")
        .map_err(internal_error("Could not back up the taxonomy before importing; nothing was imported."))?;
    let report = state
        .repo
        .import_taxonomy(&payload.taxonomy, payload.replace)
        .map_err(internal_error("Could not import the taxonomy."))?;
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!(
            "Taxonomy imported{} by {}: {} groups and {} people saved, {} groups and {} people removed",
            if payload.replace { " (replace)" } else { "" },
            admin.email,
            report.groups_saved,
            report.people_saved,
            report.groups_removed,
            report.people_removed
        ),
    );
    Ok(Json(serde_json::json!({ "status": "ok", "report": report })))
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

#[derive(Deserialize)]
pub struct UserActivePayload {
    active: bool,
}

/// Deactivate or reactivate an account (plan P2.4). A deactivated account
/// cannot sign in and its sessions end at once. The last active
/// administrator, and your own account, cannot be deactivated here: nobody
/// could administer the station afterwards.
pub async fn api_admin_set_user_active(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(user_id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(payload): Json<UserActivePayload>,
) -> JsonResult {
    let users = state.repo.list_users().map_err(internal_error("Could not read the accounts."))?;
    let Some(target) = users.iter().find(|u| u.id == user_id) else {
        return Err(ApiError::bad_request("No such account."));
    };
    if !payload.active {
        if target.email.eq_ignore_ascii_case(&admin.email) {
            return Err(ApiError::bad_request("You cannot deactivate your own account."));
        }
        let other_admins = users
            .iter()
            .filter(|u| u.id != user_id && u.is_active && u.role == UserRole::Admin)
            .count();
        if target.role == UserRole::Admin && other_admins == 0 {
            return Err(ApiError::bad_request("This is the last active administrator."));
        }
    }
    state
        .repo
        .set_user_active_status(user_id, payload.active)
        .map_err(internal_error("Could not change the account."))?;
    if !payload.active {
        let _ = state.repo.delete_sessions_for_user(user_id);
    }
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!(
            "Account {} {} by {}",
            target.email,
            if payload.active { "reactivated" } else { "deactivated" },
            admin.email
        ),
    );
    omni_core::selftest::check_default_admin(&state.repo, &state.health);
    Ok(Json(serde_json::json!({ "status": "ok" })))
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
    omni_core::selftest::check_default_admin(&state.repo, &state.health);
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

/// Put the previous yt-dlp build back (plan P2.9).
///
/// A yt-dlp release that breaks an extractor is an ordinary event, and before
/// this the newsroom's only recourse was finding the old executable by hand
/// while the queue filled up.
pub async fn api_admin_rollback_ytdl(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
) -> JsonResult {
    let bin_dir = { state.config.read().await.bin_dir.clone() };
    let dep_mgr = DependencyManager::new(&bin_dir);
    let path = dep_mgr.rollback_ytdl().map_err(|e| {
        tracing::error!(error = ?e, "yt-dlp rollback failed");
        ApiError::bad_request(e.to_string())
    })?;
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!("{} rolled yt-dlp back to the previous build", admin.email),
    );
    Ok(Json(
        serde_json::json!({ "status": "ok", "path": path.to_string_lossy() }),
    ))
}

/// The maintenance schedule and how each task last went (plan P6.6).
pub async fn api_admin_maintenance(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
) -> JsonResult {
    let specs = omni_core::scheduler::default_tasks();
    let tasks = state
        .repo
        .list_scheduled_tasks(&specs)
        .map_err(internal_error("Could not read the maintenance schedule."))?;
    Ok(Json(serde_json::json!({ "tasks": tasks })))
}

/// Bring a task's next run forward to now — the panel's "Run now".
///
/// It does not run the task inline: the scheduler owns execution, so a task
/// cannot end up running twice concurrently because someone clicked while it
/// was already due. The next tick picks it up, within a minute.
pub async fn api_admin_run_task(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
) -> JsonResult {
    let specs = omni_core::scheduler::default_tasks();
    if !specs.iter().any(|s| s.name == name) {
        return Err(ApiError::bad_request(format!("Unknown task `{name}`.")));
    }

    let scheduled = state
        .repo
        .run_task_now(&name)
        .map_err(internal_error("Could not schedule the task."))?;
    if !scheduled {
        return Err(ApiError::not_found());
    }

    let _ = state.repo.log_audit(
        "INFO",
        "ADMIN",
        &format!("{} requested maintenance task `{name}`", admin.email),
    );
    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": "Scheduled; it will start within a minute."
    })))
}

// ==========================================
// Self-check (plan P6.7)
// ==========================================

/// The self-check links with their last results, and the browser's state.
pub async fn api_admin_selfcheck(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let links = state
        .repo
        .list_selfcheck_links()
        .map_err(internal_error("Could not read the self-check links."))?;
    let checks = state.health.all();
    Ok(Json(serde_json::json!({
        "links": links,
        "summary": checks.get(omni_core::health::checks::SELFCHECK),
        "browser": checks.get(omni_core::health::checks::BROWSER),
    })))
}

#[derive(Deserialize)]
pub struct SelfcheckLinkPayload {
    label: String,
    url: String,
}

pub async fn api_admin_selfcheck_add(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<SelfcheckLinkPayload>,
) -> JsonResult {
    let url = payload.url.trim();
    let label: String = payload.label.trim().chars().take(80).collect();
    if !is_submittable_url(url) {
        return Err(ApiError::bad_request("Enter an http:// or https:// link."));
    }
    if label.is_empty() {
        return Err(ApiError::bad_request("Give the link a name, such as \"Instagram reel\"."));
    }
    let id = state
        .repo
        .add_selfcheck_link(&label, url)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let _ = state.repo.log_audit("INFO", "ADMIN", &format!("{} added self-check link {label}: {url}", admin.email));
    Ok(Json(serde_json::json!({ "status": "ok", "id": id })))
}

pub async fn api_admin_selfcheck_delete(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    let removed = state
        .repo
        .delete_selfcheck_link(id)
        .map_err(internal_error("Could not remove the link."))?;
    if !removed {
        return Err(ApiError::not_found());
    }
    let _ = state.repo.log_audit("INFO", "ADMIN", &format!("{} removed self-check link #{id}", admin.email));
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ==========================================
// Secrets API (plan P2.6)
// ==========================================

/// Which secrets are configured — **never their values**.
///
/// There is deliberately no read route. An admin session that has been taken
/// over can overwrite the mailbox password (which is loud: mail ingest stops)
/// but cannot walk away with it.
pub async fn api_secrets_status(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
) -> JsonResult {
    Ok(Json(serde_json::json!({ "secrets": state.secrets.status() })))
}

#[derive(Deserialize)]
pub struct SetSecretPayload {
    key: String,
    value: String,
}

pub async fn api_secrets_set(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<SetSecretPayload>,
) -> JsonResult {
    if !omni_core::secrets::keys::ALL.contains(&payload.key.as_str()) {
        // Only the known keys, so a typo cannot quietly write a secret that
        // nothing will ever read.
        return Err(ApiError::bad_request(format!(
            "Unknown secret `{}`.",
            payload.key
        )));
    }

    state
        .secrets
        .set(&payload.key, payload.value.trim())
        .map_err(internal_error("Could not store the secret."))?;

    // The key is logged; the value is not, and never will be.
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!(
            "Secret `{}` {} by {}",
            payload.key,
            if payload.value.trim().is_empty() { "cleared" } else { "updated" },
            admin.email
        ),
    );

    // Mail uses its secret from the running config, so reflect the change
    // without a restart.
    if payload.key == omni_core::secrets::keys::GRAPH_CLIENT_SECRET {
        let mut cfg = state.config.write().await;
        cfg.graph.client_secret = payload.value.trim().to_string();
    }

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ==========================================
// System Status & Setup API
// ==========================================

/// Public health, for an external monitor (plan P6.2).
///
/// A verdict and each check's state, and nothing else: no paths, no versions,
/// no error strings. A monitor needs to know *that* something is wrong and who
/// to page; an operator opens the panel to find out what. The route is
/// unauthenticated, so everything it says is said to anyone who can reach the
/// port.
///
/// The status code follows the verdict, because half of monitoring tools only
/// look at that: `down` answers 503 so a health check fails rather than
/// reporting a cheerful 200 with bad news in the body.
pub async fn api_health(State(state): State<AppState>) -> Response {
    let mut body = state.health.summary();
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "version".into(),
            serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
        );
        obj.insert(
            "uptime_secs".into(),
            serde_json::Value::from(state.health.uptime_secs()),
        );
    }

    let code = match state.health.overall() {
        omni_core::health::Health::Down => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::OK,
    };
    (code, Json(body)).into_response()
}

/// The full picture, for the MCR panel (plan P6.2, defect W-09).
///
/// This replaces two hardcoded string literals — `mail_status: "Active"` and
/// `llm_status: "Ready"` — that were true when they were written and never
/// checked again. A panel that reports a healthy mailbox while the mailbox is
/// refusing the password is worse than a panel that reports nothing, because an
/// operator who trusts it stops looking there.
pub async fn api_system_status(
    RequireMcr(_): RequireMcr,
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    use omni_core::selftest::{disk_space, BYTES_PER_GB};

    let (watchfolder, temp_path, bin_dir, llm_model) = {
        let cfg = state.config.read().await;
        (
            cfg.watchfolder_path.clone(),
            cfg.temp_path.clone(),
            cfg.bin_dir.clone(),
            cfg.ollama_model.clone(),
        )
    };

    let disk_of = |p: &str| {
        disk_space(std::path::Path::new(p))
            .map(|(free, total)| {
                serde_json::json!({
                    "free_gb": (free as f64 / BYTES_PER_GB * 10.0).round() / 10.0,
                    "total_gb": (total as f64 / BYTES_PER_GB * 10.0).round() / 10.0,
                })
            })
            .unwrap_or(serde_json::Value::Null)
    };

    // `null` when the count failed, never zeros: the desk alerts on a rise in
    // the review count, and zeros followed by the real count would announce
    // the whole backlog as new (P7.14).
    let queue = state.repo.queue_summary().ok();

    let checks = state.health.all();

    Json(serde_json::json!({
        "status": state.health.overall().as_str(),
        "checks": checks,
        "queue": queue,
        "disk": {
            "watchfolder": disk_of(&watchfolder),
            "temp": disk_of(&temp_path),
        },
        "service": {
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": state.health.uptime_secs(),
            "started_at": state.health.started_at(),
            "tls": state.tls_enabled,
        },
        "config": {
            "watchfolder_path": watchfolder,
            "bin_dir": bin_dir,
            "llm_model": llm_model,
        },

        // Kept so an older cached panel does not break on upgrade. Derived
        // from the real checks now, not hardcoded.
        "free_disk_gb": disk_space(std::path::Path::new(&watchfolder))
            .map(|(free, _)| free as f64 / BYTES_PER_GB)
            .unwrap_or(0.0),
        "mail_status": legacy_label(&state, omni_core::health::checks::MAIL),
        "llm_status": legacy_label(&state, omni_core::health::checks::LLM),
    }))
}

/// The old two-word status strings, derived from the real check.
fn legacy_label(state: &AppState, check: &str) -> &'static str {
    match state.health.get(check).map(|c| c.state) {
        Some(omni_core::health::Health::Ok) => "Active",
        Some(omni_core::health::Health::Degraded) => "Degraded",
        Some(omni_core::health::Health::Down) => "Down",
        None => "Unknown",
    }
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

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct TestEmailPayload {
    tenant_id: String,
    client_id: String,
    mailbox: String,
    /// Blank: test with the stored secret, which is then only ever paired
    /// with the stored tenant and client id, never with typed-in ones.
    client_secret: String,
}

/// Test the Graph mailbox (plan P4.7): sign in and read the inbox.
///
/// Admin-only: unauthenticated, this was a credential oracle — point it at the
/// station's mailbox and the response distinguishes a right password from a
/// wrong one, for free, from anywhere on the LAN.
pub async fn api_test_email(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<TestEmailPayload>,
) -> JsonResult {
    let saved = state.config.read().await.graph.clone();
    let pick = |typed: &str, saved: &str| {
        let typed = typed.trim();
        if typed.is_empty() { saved.to_string() } else { typed.to_string() }
    };
    let mut cfg = saved.clone();
    cfg.mailbox = pick(&payload.mailbox, &saved.mailbox);
    if !payload.client_secret.trim().is_empty() {
        cfg.tenant_id = pick(&payload.tenant_id, &saved.tenant_id);
        cfg.client_id = pick(&payload.client_id, &saved.client_id);
        cfg.client_secret = payload.client_secret.trim().to_string();
    }
    match omni_email::graph::GraphMailSource::new(cfg).test_access().await {
        Ok(detail) => Ok(Json(serde_json::json!({ "status": "ok", "detail": detail }))),
        Err(e) => Err(ApiError::bad_request(e.to_string())),
    }
}

// ==========================================
// MCR mail view (plan P7.8)
// ==========================================

#[derive(Deserialize)]
pub struct MailsQuery {
    #[serde(default)]
    filter: omni_email::inbox::InboxFilter,
    q: Option<String>,
    page: Option<usize>,
    per_page: Option<usize>,
    /// `mail:<Message-ID>` or `manual:<id>`: return the page that holds it.
    focus: Option<String>,
}

/// One page of the mail view's list: handled mail and links added by hand,
/// newest first, within the days the mail text is kept.
/// The Email tab's entries received since `since`.
fn inbox_entries(state: &AppState, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<omni_email::inbox::Entry>, ApiError> {
    let mails = state.repo.inbox_mail_rows(since).map_err(internal_error("Could not read the mail list."))?;
    let extra = omni_email::inbox::referenced_job_ids(&mails);
    // A day of slack for jobs: a mail's received time is the server's clock.
    let jobs = state
        .repo
        .inbox_job_rows(since - chrono::Duration::days(1), &extra)
        .map_err(internal_error("Could not read the mail list."))?;
    Ok(omni_email::inbox::entries(mails, jobs, since))
}

/// Recent emails (and links added by hand) with whether all their videos
/// have ended (plan P5.4). The desk polls this on every tab and chimes for
/// an entry that turned settled. Two days back: what is still in play.
pub async fn api_get_mail_settlements(RequireMcr(_): RequireMcr, State(state): State<AppState>) -> JsonResult {
    let since = chrono::Utc::now() - chrono::Duration::days(2);
    let entries = inbox_entries(&state, since)?;
    Ok(Json(serde_json::json!({ "entries": omni_email::inbox::settlements(&entries, since) })))
}

pub async fn api_get_mails(
    RequireMcr(_): RequireMcr,
    Query(query): Query<MailsQuery>,
    State(state): State<AppState>,
) -> JsonResult {
    let days = state.config.read().await.mail_text_retention_days.clamp(1, 3650);
    let since = chrono::Utc::now() - chrono::Duration::days(days);
    let entries = inbox_entries(&state, since)?;
    let page = omni_email::inbox::page_with_focus(
        entries,
        query.filter,
        query.q.as_deref().unwrap_or(""),
        query.page.unwrap_or(1),
        query.per_page.unwrap_or(20),
        query.focus.as_deref(),
    );

    // Who added a link by hand, by name.
    let names: HashMap<i64, String> = if page.entries.iter().any(|e| e.added_by.is_some()) {
        state
            .repo
            .list_users()
            .map_err(internal_error("Could not read the mail list."))?
            .into_iter()
            .map(|u| (u.id, u.full_name))
            .collect()
    } else {
        HashMap::new()
    };
    let entries: Vec<serde_json::Value> = page
        .entries
        .iter()
        .map(|e| {
            let mut v = serde_json::to_value(e).unwrap_or_default();
            if let Some(name) = e.added_by.and_then(|id| names.get(&id)) {
                v["added_by_name"] = serde_json::Value::String(name.clone());
            }
            v
        })
        .collect();
    let job_counts = state.repo.job_counts().map_err(internal_error("Could not count the jobs."))?;
    Ok(Json(serde_json::json!({
        "entries": entries,
        "total": page.total,
        "page": page.page,
        "per_page": page.per_page,
        "counts": page.counts,
        "job_counts": job_counts,
        "window_days": days,
    })))
}

#[derive(Deserialize)]
pub struct MailViewQuery {
    /// `mail` (default) or `manual`.
    kind: Option<String>,
    key: String,
    /// `jobs`: only what changes while the mail is open, for the live refresh.
    parts: Option<String>,
}

/// A link added by hand, shaped as a mail whose text is the link, so the
/// desk shows it the way it shows mail.
fn manual_entry_as_mail(root: &omni_core::models::Job) -> omni_core::models::ProcessedMail {
    omni_core::models::ProcessedMail {
        internet_message_id: String::new(),
        outcome: "JOBS".into(),
        subject: Some(root.url.clone()),
        jobs_json: serde_json::json!([{
            "index_str": root.index_str,
            "slug": root.slug,
            "url": root.url,
            "status": root.status.as_str(),
            "result": { "outcome": "created", "id": root.id },
        }])
        .to_string(),
        received_at: root.created_at,
        body_text: Some(root.url.clone()),
        ..Default::default()
    }
}

/// The mail (or manual entry) `key` and its jobs.
fn load_mail(
    state: &AppState,
    kind: Option<&str>,
    key: &str,
) -> Result<(omni_core::models::ProcessedMail, Vec<omni_core::models::Job>), ApiError> {
    if kind == Some("manual") {
        let id: i64 = key.trim().parse().map_err(|_| ApiError::not_found())?;
        let jobs = state.repo.jobs_with_children(id).map_err(internal_error("Could not read the job."))?;
        let root = jobs
            .iter()
            .find(|j| j.id == id && j.email_message_id.is_none())
            .ok_or_else(ApiError::not_found)?;
        return Ok((manual_entry_as_mail(root), jobs));
    }
    let mail = state
        .repo
        .get_processed_mail(key)
        .map_err(internal_error("Could not read the mail."))?
        .ok_or_else(ApiError::not_found)?;
    let pointed_at: Vec<i64> = serde_json::from_str::<Vec<omni_email::watcher::QueuedFromMail>>(&mail.jobs_json)
        .unwrap_or_default()
        .iter()
        .map(|q| q.result.job_id())
        .collect();
    let jobs = state
        .repo
        .jobs_for_mail(&mail.internet_message_id, &pointed_at)
        .map_err(internal_error("Could not read the mail's jobs."))?;
    Ok((mail, jobs))
}

/// The videos an article offered MCR and nobody has queued yet, per job.
fn open_offers(jobs: &[omni_core::models::Job]) -> Vec<serde_json::Value> {
    jobs.iter()
        .flat_map(|j| {
            j.candidates_json
                .as_deref()
                .and_then(|c| serde_json::from_str::<Vec<omni_broadcast::article::Offered>>(c).ok())
                .unwrap_or_default()
                .into_iter()
                .filter(|o| o.queued_job_id.is_none())
                .map(move |o| serde_json::json!({ "job_id": j.id, "url": o.url, "index_str": o.index_str }))
        })
        .collect()
}

pub async fn api_get_mail_view(
    RequireMcr(_): RequireMcr,
    Query(query): Query<MailViewQuery>,
    State(state): State<AppState>,
) -> JsonResult {
    let cfg = state.config.read().await.parser.clone();
    let (mail, jobs) = load_mail(&state, query.kind.as_deref(), &query.key)?;
    let view = omni_email::mail_view::build(&mail, &jobs, &cfg);

    let by_id: HashMap<i64, &omni_core::models::Job> = jobs.iter().map(|j| (j.id, j)).collect();
    let desk_jobs: Vec<serde_json::Value> = view
        .jobs
        .iter()
        .filter_map(|p| {
            by_id.get(&p.job_id).map(|j| {
                let mut v = job_for_desk(j);
                v["place"] = serde_json::json!({ "link": p.link, "parent": p.parent, "shared": p.shared });
                v
            })
        })
        .collect();
    let links: Vec<serde_json::Value> = view
        .links
        .iter()
        .map(|l| serde_json::json!({ "id": l.id, "url": l.url, "role": l.role, "jobs": l.jobs, "skip": l.skip }))
        .collect();

    if query.parts.as_deref() == Some("jobs") {
        return Ok(Json(serde_json::json!({
            "jobs": desk_jobs,
            "links": links,
            "attachments": view.attachments,
            "offers": open_offers(&jobs),
            "next_index": view.next_index,
        })));
    }

    let summary = omni_email::mail_view::MailParseSummary::stored(&mail);
    Ok(Json(serde_json::json!({
        "kind": if query.kind.as_deref() == Some("manual") { "manual" } else { "mail" },
        "key": query.key,
        "subject": mail.subject,
        "from_name": mail.from_name,
        "from_address": mail.from_address,
        "to": mail.to,
        "cc": mail.cc,
        "received_at": mail.received_at,
        "processed_at": mail.processed_at,
        "outcome": mail.outcome,
        "journalist": summary.as_ref().map(|s| s.journalist.clone()).or_else(|| jobs.first().map(|j| j.journalist.clone())),
        "how": summary.as_ref().map(|s| omni_email::mail_view::how_el(s.how)),
        "group_code": summary.as_ref().and_then(|s| s.group.as_ref().map(|g| g.code.clone())),
        "urgent": summary.as_ref().is_some_and(|s| s.urgent),
        "notes": view.notes,
        "text": view.text,
        "links": links,
        "attachments": view.attachments,
        "jobs": desk_jobs,
        "offers": open_offers(&jobs),
        "next_index": view.next_index,
    })))
}

#[derive(Deserialize)]
pub struct QueueLinkPayload {
    key: String,
    url: String,
}

/// Queue a link from a mail's text that became no job: «Λήψη και αυτού».
///
/// Only a link that is in that mail's stored text, is not already a job,
/// and is not a photo or a document. It is named as the mail's own links
/// are (journalist, the next number, the neighbouring story's keyword) and
/// recorded with the mail, so the desk files the job under that link.
pub async fn api_mail_queue_link(
    RequireMcr(principal): RequireMcr,
    State(state): State<AppState>,
    Json(payload): Json<QueueLinkPayload>,
) -> JsonResult {
    let cfg = state.config.read().await.parser.clone();
    let (mail, jobs) = load_mail(&state, None, &payload.key)?;
    let view = omni_email::mail_view::build(&mail, &jobs, &cfg);
    let wanted = payload.url.trim();
    let Some(link) = view.links.iter().find(|l| l.url == wanted) else {
        return Err(ApiError::bad_request("Αυτός ο σύνδεσμος δεν υπάρχει στο κείμενο αυτού του email."));
    };
    match &link.skip {
        None => return Err(ApiError::bad_request("Αυτός ο σύνδεσμος έχει ήδη μπει στην ουρά.")),
        Some(s) if !s.can_queue => {
            return Err(ApiError::bad_request("Φωτογραφίες και έγγραφα δεν κατεβαίνουν ως βίντεο."))
        }
        Some(_) => {}
    }
    if !is_submittable_url(&link.url) {
        return Err(ApiError::bad_request("Επικολλήστε έναν σύνδεσμο που αρχίζει με http:// ή https://."));
    }
    let (journalist, keyword, index) = omni_email::mail_view::naming_for_link(&mail, &view, &jobs, &link.id)
        .ok_or_else(|| ApiError::bad_request("Αυτός ο σύνδεσμος δεν υπάρχει στο κείμενο αυτού του email."))?;
    let summary = omni_email::mail_view::MailParseSummary::stored(&mail);
    let subject = mail.subject.clone().unwrap_or_default();
    let slug = format!("{index}_{journalist}_{keyword}");

    let locker = omni_email::parser::classify(&link.url, &cfg) == omni_email::parser::Tier::Locker;
    let mut new = omni_core::models::NewJob::new(link.url.clone(), slug.clone(), journalist.clone());
    new.keyword = keyword;
    new.index_str = index.clone();
    new.status = if locker { JobStatus::ManualDownload } else { JobStatus::Pending };
    new.extraction_method = locker.then(|| "locker".to_string());
    new.email_message_id = Some(mail.internet_message_id.clone());
    new.email_source = mail.from_address.clone();
    new.group_code = summary
        .as_ref()
        .and_then(|s| s.group.as_ref().map(|g| g.code.clone()))
        .or_else(|| jobs.iter().find_map(|j| j.group_code.clone()));
    new.priority = if summary.as_ref().is_some_and(|s| s.urgent) {
        omni_email::watcher::URGENT_PRIORITY_BOOST
    } else {
        0
    };
    new.submitted_by_user_id = principal.user().map(|u| u.id);
    new.notes = Some(format!("Queued by MCR from the email: {subject}"));
    let result = state
        .repo
        .enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
        .map_err(internal_error("Could not queue that link."))?;
    state
        .repo
        .append_mail_job(
            &mail.internet_message_id,
            &serde_json::json!({
                "index_str": index,
                "slug": slug,
                "url": link.url,
                "status": new.status.as_str(),
                "result": result,
            }),
        )
        .map_err(internal_error("Could not record the link with its email."))?;
    let job_id = result.job_id();
    if result.is_new() {
        let _ = state.repo.record_event(
            job_id,
            "INFO",
            None,
            &format!("Queued by {} from the email '{subject}'", principal.audit_label()),
        );
    }
    audit_action(&state, &principal, &format!("Job #{job_id}: queued from the email '{subject}'"));
    state.broadcast_event("job_created");
    Ok(Json(serde_json::json!({
        "status": "ok",
        "job_id": job_id,
        "slug": slug,
        "index_str": index,
        "duplicate": !result.is_new(),
    })))
}

#[derive(Deserialize)]
pub struct MailKeyPayload {
    key: String,
}

/// Read again a mail the watcher gave up on. Only that: a handled mail read
/// again after a day could queue its delivered links a second time, so that
/// stays with the administrator (plan P4.25).
pub async fn api_mail_reprocess(
    RequireMcr(principal): RequireMcr,
    State(state): State<AppState>,
    Json(payload): Json<MailKeyPayload>,
) -> JsonResult {
    let mail = state
        .repo
        .get_processed_mail(payload.key.trim())
        .map_err(internal_error("Could not read the mail."))?
        .ok_or_else(ApiError::not_found)?;
    if mail.outcome != "FAILED" {
        return Err(ApiError::bad_request(
            "Ξανά διαβάζεται μόνο ένα email που το σύστημα δεν μπόρεσε να διαβάσει.",
        ));
    }
    state
        .repo
        .request_mail_reprocess(&mail.internet_message_id, &principal.audit_label())
        .map_err(|_| ApiError::bad_request("Αυτό το email δεν μπορεί να διαβαστεί ξανά από το γραμματοκιβώτιο."))?;
    audit_action(
        &state,
        &principal,
        &format!("Reprocess of the email '{}' requested", mail.subject.unwrap_or_default()),
    );
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ==========================================
// Handled mail and reprocess (plan P4.25)
// ==========================================

pub async fn api_admin_mail_history(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let rows = state.repo.list_processed_mail(100).map_err(internal_error("Could not read the mail history."))?;
    let pending: Vec<String> = state
        .repo
        .pending_mail_reprocess()
        .map_err(internal_error("Could not read the mail history."))?
        .into_iter()
        .map(|(k, _, _)| k)
        .collect();
    let mails: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|m| {
            let jobs = serde_json::from_str::<Vec<serde_json::Value>>(&m.jobs_json).map(|v| v.len()).unwrap_or(0);
            serde_json::json!({
                "internet_message_id": m.internet_message_id,
                "subject": m.subject,
                "from_address": m.from_address,
                "outcome": m.outcome,
                "processed_at": m.processed_at,
                "jobs": jobs,
                "reprocess_pending": pending.contains(&m.internet_message_id),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "mails": mails })))
}

#[derive(Deserialize)]
pub struct ReprocessPayload {
    internet_message_id: String,
}

pub async fn api_admin_mail_reprocess(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<ReprocessPayload>,
) -> JsonResult {
    let m = state
        .repo
        .request_mail_reprocess(payload.internet_message_id.trim(), &admin.email)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let _ = state.repo.log_audit(
        "INFO",
        "EMAIL",
        &format!("Reprocess of '{}' requested by {}", m.subject.unwrap_or_default(), admin.email),
    );
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ==========================================
// LLM settings (plan P4.22)
// ==========================================

/// The LLM card of the admin panel: what is being saved, tested or asked
/// for its model list. Nothing here is applied until it is saved.
#[derive(Deserialize)]
pub struct LlmSettingsPayload {
    #[serde(default)]
    mode: omni_core::config::LlmMode,
    #[serde(default)]
    provider: String,
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    auth: omni_core::config::LlmAuth,
    /// Blank: keep the stored key, which is then only ever sent to the
    /// stored base URL, never to a typed-in one.
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    clear_key: bool,
    #[serde(default = "default_llm_timeout_payload")]
    timeout_secs: u64,
    #[serde(default = "default_llm_tokens_payload")]
    max_tokens: u32,
    #[serde(default = "default_true_payload")]
    disable_thinking: bool,
    /// The LLM writes every section's keyword, not only those the parser
    /// could not make.
    #[serde(default)]
    keyword_polish: bool,
}

fn default_llm_timeout_payload() -> u64 {
    30
}
fn default_llm_tokens_payload() -> u32 {
    400
}
fn default_true_payload() -> bool {
    true
}

fn same_base(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/').eq_ignore_ascii_case(b.trim().trim_end_matches('/'))
}

/// Endpoint, model and LLM config for `p`, on top of the saved config.
fn llm_settings(
    p: &LlmSettingsPayload,
    saved: &omni_core::config::AppConfig,
) -> Result<(String, String, omni_core::config::LlmConfig), ApiError> {
    let base = p.base_url.trim().to_string();
    omni_email::llm::check_base_url(&base).map_err(ApiError::bad_request)?;
    let key = if !p.api_key.trim().is_empty() {
        p.api_key.trim().to_string()
    } else if p.clear_key {
        String::new()
    } else if same_base(&base, &saved.ollama_endpoint) {
        saved.llm.api_key.clone()
    } else {
        String::new()
    };
    let cfg = omni_core::config::LlmConfig {
        mode: p.mode,
        timeout_secs: p.timeout_secs.clamp(5, 300),
        max_tokens: p.max_tokens.clamp(50, 8000),
        keyword_polish: p.keyword_polish,
        provider: {
            let v = p.provider.trim().to_lowercase();
            if v.is_empty() { "custom".into() } else { v.chars().take(30).collect() }
        },
        auth: p.auth,
        disable_thinking: p.disable_thinking,
        api_key: key,
    };
    Ok((base, p.model.trim().chars().take(200).collect(), cfg))
}

pub async fn api_admin_get_llm(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let cfg = state.config.read().await;
    Ok(Json(serde_json::json!({
        "mode": cfg.llm.mode,
        "provider": cfg.llm.provider,
        "base_url": cfg.ollama_endpoint,
        "model": cfg.ollama_model,
        "auth": cfg.llm.auth,
        "timeout_secs": cfg.llm.timeout_secs,
        "max_tokens": cfg.llm.max_tokens,
        "disable_thinking": cfg.llm.disable_thinking,
        "keyword_polish": cfg.llm.keyword_polish,
        "key_set": !cfg.llm.api_key.is_empty(),
        "online": !omni_email::llm::is_local_endpoint(&cfg.ollama_endpoint),
        "in_use": state.llm.current().describe(),
    })))
}

/// Save and apply: the watcher uses the new settings from its next mail.
pub async fn api_admin_save_llm(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<LlmSettingsPayload>,
) -> JsonResult {
    let mut cfg = state.config.write().await;
    let (base, model, llm) = llm_settings(&payload, &cfg)?;
    if llm.mode != omni_core::config::LlmMode::Off && model.is_empty() {
        return Err(ApiError::bad_request("Choose a model (Load models lists what the server offers)."));
    }
    // The key first: if it cannot be stored, nothing changes.
    let key_key = omni_core::secrets::keys::LLM_API_KEY;
    if !payload.api_key.trim().is_empty() {
        state.secrets.set(key_key, payload.api_key.trim()).map_err(internal_error("Could not store the API key."))?;
    } else if payload.clear_key {
        state.secrets.remove(key_key).map_err(internal_error("Could not clear the API key."))?;
    } else if !same_base(&base, &cfg.ollama_endpoint) && !cfg.llm.api_key.is_empty() {
        // A new provider does not inherit the old one's key.
        state.secrets.remove(key_key).map_err(internal_error("Could not clear the API key."))?;
    }
    cfg.ollama_endpoint = base;
    cfg.ollama_model = model;
    cfg.llm = llm;
    cfg.save_to_file(&state.config_path).map_err(internal_error("Could not save the configuration."))?;
    let assist = omni_email::assist::Assist::from_config(&cfg);
    let described = assist.describe();
    state.llm.replace(assist);
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!("LLM settings changed by {}: {:?}, {described}", admin.email, cfg.llm.mode),
    );
    Ok(Json(serde_json::json!({ "status": "ok", "in_use": described })))
}

/// A real, small JSON request with the unsaved settings, timed.
pub async fn api_admin_test_llm(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<LlmSettingsPayload>,
) -> JsonResult {
    let saved = state.config.read().await.clone();
    let (base, model, llm) = llm_settings(&payload, &saved)?;
    if model.is_empty() {
        return Err(ApiError::bad_request("Choose a model first."));
    }
    let max_tokens = llm.max_tokens;
    let client = omni_email::llm::LlmClient::from_settings(&base, &model, &llm);
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "word": { "type": "string" } },
        "required": ["word"],
        "additionalProperties": false
    });
    let started = std::time::Instant::now();
    let answer = client
        .chat_json(
            "Answer with one JSON object and nothing else: {\"word\": string}.",
            "Transliterate the Greek word ΛΙΜΑΝΙ to uppercase Latin letters.",
            &schema,
            max_tokens,
        )
        .await
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({
        "status": "ok",
        "seconds": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        "answer": answer,
        "online": client.is_online(),
    })))
}

/// What models the server offers, with the unsaved settings.
pub async fn api_admin_llm_models(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<LlmSettingsPayload>,
) -> JsonResult {
    let saved = state.config.read().await.clone();
    let (base, model, llm) = llm_settings(&payload, &saved)?;
    let models = omni_email::llm::LlmClient::from_settings(&base, &model, &llm)
        .list_models()
        .await
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "models": models })))
}

#[derive(Deserialize)]
pub struct SetupPayload {
    /// The Graph mailbox (plan P4.7). All blank: no email ingest.
    #[serde(default)]
    graph_tenant_id: String,
    #[serde(default)]
    graph_client_id: String,
    #[serde(default)]
    graph_mailbox: String,
    /// Blank keeps the stored secret.
    #[serde(default)]
    graph_client_secret: String,
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
        cfg.graph.tenant_id = payload.graph_tenant_id.trim().to_string();
        cfg.graph.client_id = payload.graph_client_id.trim().to_string();
        cfg.graph.mailbox = payload.graph_mailbox.trim().to_string();
        let secret = payload.graph_client_secret.trim();
        if !secret.is_empty() {
            // Into the encrypted store, never into config.json (plan P2.6).
            // The runtime copy is what the mail watcher reads.
            if let Err(e) = state
                .secrets
                .set(omni_core::secrets::keys::GRAPH_CLIENT_SECRET, secret)
            {
                tracing::error!(error = ?e, "Storing the Graph client secret failed");
                return ApiError::internal("Could not store the Graph client secret.")
                    .into_response();
            }
            cfg.graph.client_secret = secret.to_string();
        }
        cfg.ollama_endpoint = payload.ollama_endpoint;
        cfg.ollama_model = payload.ollama_model;
        cfg.watchfolder_path = payload.watchfolder_path;
        if let Err(e) = cfg.save_to_file(&state.config_path) {
            tracing::error!(error = ?e, "Saving config failed");
            return ApiError::internal("Could not save the configuration.").into_response();
        }
        state.llm.replace(omni_email::assist::Assist::from_config(&cfg));
    }
    let _ = state
        .repo
        .log_audit("WARN", "ADMIN", "Configuration changed via /setup");

    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Whether the first-run page should still be offered in the UI.
/// Public, because the login page links to it and must know.
///
/// To whoever may use the page (an admin, or loopback before the first admin)
/// it also returns the current mailbox settings, so a reopened page does not
/// blank them. Never a secret: only whether one is set.
pub async fn api_setup_state(State(state): State<AppState>, req: Request) -> Json<serde_json::Value> {
    let (parts, _) = req.into_parts();
    let mut out = serde_json::json!({
        "needs_admin": !state.repo.has_active_admin().unwrap_or(true)
    });
    if setup_window_open(&parts, &state).await {
        let cfg = state.config.read().await;
        out["graph"] = serde_json::json!({
            "tenant_id": cfg.graph.tenant_id,
            "client_id": cfg.graph.client_id,
            "mailbox": cfg.graph.mailbox,
            "secret_set": !cfg.graph.client_secret.is_empty(),
        });
        out["watchfolder_path"] = serde_json::json!(cfg.watchfolder_path);
        out["ollama_endpoint"] = serde_json::json!(cfg.ollama_endpoint);
        out["ollama_model"] = serde_json::json!(cfg.ollama_model);
    }
    Json(out)
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
        assert_eq!(sanitize_token("Σεισμός", "ASSET"), "SEISMOS");
        assert_eq!(sanitize_token("Παπαδάκη", "MCR"), "PAPADAKI");
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
