//! The journalist's own panel API (plan P7.3).
//!
//! A plain `user` account may submit links, see and cancel its *own* waiting
//! videos and keep one preference. Everything here is scoped to the account in
//! the session: the journalist name comes from the account record, never from
//! the request, and a job id that is not the caller's answers 404 exactly as a
//! missing one does.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use omni_core::models::{JobStatus, NewJob, User};
use omni_core::repository::OwnCancelOutcome;

use crate::auth::{internal_error, ApiError, RequireUser};
use crate::routes::{is_submittable_url, sanitize_token};
use crate::state::AppState;

type JsonResult = Result<Json<serde_json::Value>, ApiError>;

/// Links one request may carry; 1A..1T is as far as the suffix letters go.
const MAX_LINKS: usize = 20;
/// The priority an "urgent" tick means; the top of the scale.
const URGENT_PRIORITY: i32 = 10;
const MAX_NOTES_LEN: usize = 300;

#[derive(Deserialize)]
pub struct SubmitPayload {
    url: Option<String>,
    urls: Option<Vec<String>>,
    keyword: String,
    notes: Option<String>,
    priority: Option<bool>,
    index_str: Option<String>,
}

fn index_suffix(n: usize) -> char {
    (b'A' + (n % 26) as u8) as char
}

/// The account's own surname, the same fallback the MCR form uses.
fn journalist_of(user: &User) -> String {
    sanitize_token(user.journalist_surname.as_deref().unwrap_or("MCR"), "MCR")
}

/// `POST /api/user/jobs`: one link, or up to twenty, as the signed-in journalist.
pub async fn api_user_submit(
    RequireUser(user): RequireUser,
    State(state): State<AppState>,
    Json(payload): Json<SubmitPayload>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let urls: Vec<String> = match (payload.url, payload.urls) {
        (Some(u), None) => vec![u],
        (None, Some(list)) => list,
        _ => return Err(ApiError::bad_request("Στείλτε είτε έναν σύνδεσμο είτε μια λίστα συνδέσμων.")),
    };
    let urls: Vec<String> = urls.into_iter().map(|u| u.trim().to_string()).collect();
    if urls.is_empty() || urls.len() > MAX_LINKS {
        return Err(ApiError::bad_request(format!(
            "Στείλτε από 1 έως {MAX_LINKS} συνδέσμους κάθε φορά."
        )));
    }
    // Validate everything before anything is queued: a list is all or nothing.
    if let Some(bad) = urls.iter().find(|u| !is_submittable_url(u)) {
        let shown: String = bad.chars().take(80).collect();
        return Err(ApiError::bad_request(format!(
            "Ο σύνδεσμος «{shown}» δεν είναι έγκυρος· πρέπει να αρχίζει με http:// ή https://. Δεν στάλθηκε τίποτα."
        )));
    }

    let journalist = journalist_of(&user);
    let keyword = sanitize_token(&payload.keyword, "ASSET");
    let base_index = sanitize_token(payload.index_str.as_deref().unwrap_or("1"), "1");
    let notes = payload
        .notes
        .as_deref()
        .map(|n| n.trim().chars().take(MAX_NOTES_LEN).collect::<String>())
        .filter(|n| !n.is_empty());
    let priority = if payload.priority.unwrap_or(false) { URGENT_PRIORITY } else { 0 };
    let multi = urls.len() > 1;

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(urls.len());
    let mut failed = false;
    for (i, url) in urls.iter().enumerate() {
        let index = if multi { format!("{base_index}{}", index_suffix(i)) } else { base_index.clone() };
        let slug = format!("{index}_{journalist}_{keyword}");
        let mut new = NewJob::new(url.as_str(), slug.clone(), journalist.clone());
        new.keyword = keyword.clone();
        new.index_str = index;
        new.priority = priority;
        new.status = JobStatus::Pending;
        new.submitted_by_user_id = Some(user.id);
        new.notes = notes.clone();
        let outcome = match state
            .repo
            .enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
        {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(error = ?e, "Enqueue failed");
                failed = true;
                break;
            }
        };
        let job_id = outcome.job_id();
        if outcome.is_new() {
            results.push(serde_json::json!({ "job_id": job_id, "slug": slug, "duplicate": false }));
            continue;
        }
        // A duplicate of the caller's own job names it. One that belongs to
        // someone else (or to MCR) says only that the link is already queued:
        // its id and slug would give away another journalist's name and keyword.
        let own = match state.repo.get_job(job_id) {
            Ok(j) => j.filter(|j| j.submitted_by_user_id == Some(user.id)),
            Err(e) => {
                tracing::error!(error = ?e, "Could not read the job");
                failed = true;
                break;
            }
        };
        results.push(match own {
            Some(j) => serde_json::json!({ "job_id": j.id, "slug": j.slug, "duplicate": true }),
            None => serde_json::json!({
                "job_id": null, "slug": null, "duplicate": true,
                "message": "Ο σύνδεσμος υπάρχει ήδη στην ουρά.",
            }),
        });
    }

    // What was created is audited and announced even when a later link failed.
    let created = results.iter().filter(|r| r["duplicate"] == false).count();
    let _ = state.repo.log_audit(
        "INFO",
        "USER",
        &format!("{} (#{}) submitted {} link(s), {} new", user.email, user.id, urls.len(), created),
    );
    if created > 0 {
        state.broadcast_event("job_created");
    }
    let (status, body) = submit_response(results, urls.len(), failed);
    Ok((status, Json(body)))
}

/// 200 with one entry per link; after a failure part-way, 500 whose body lists
/// the links done so far, the one that failed and how many were not attempted.
fn submit_response(mut results: Vec<serde_json::Value>, total: usize, failed: bool) -> (StatusCode, serde_json::Value) {
    if !failed {
        return (StatusCode::OK, serde_json::Value::Array(results));
    }
    let not_attempted = total.saturating_sub(results.len() + 1);
    results.push(serde_json::json!({
        "job_id": null, "slug": null, "duplicate": false,
        "error": "Ο σύνδεσμος δεν μπήκε στην ουρά.",
    }));
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({
            "error": { "code": "PARTIAL_FAILURE", "message": "Κάποιοι σύνδεσμοι δεν μπήκαν στην ουρά· δείτε τη λίστα." },
            "results": results,
            "not_attempted": not_attempted,
        }),
    )
}

/// `POST /api/user/jobs/:id/cancel`: only the caller's own, still waiting video.
pub async fn api_user_cancel(
    RequireUser(user): RequireUser,
    AxumPath(job_id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    match state
        .repo
        .cancel_own_pending(job_id, user.id)
        .map_err(internal_error("Could not cancel the job."))?
    {
        OwnCancelOutcome::Cancelled => {}
        // Someone else's job looks exactly like a missing one.
        OwnCancelOutcome::NotFound => return Err(ApiError::not_found()),
        OwnCancelOutcome::NotCancellable => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "NOT_CANCELLABLE",
                "Ακυρώνεται μόνο βίντεο που περιμένει ακόμη στην ουρά. Αν χρειάζεται, ζητήστε το από το MCR.",
            ));
        }
    }
    // Stop a worker that is just taking the job, as the MCR cancel does.
    state.busy_jobs.cancel(job_id);
    let _ = state
        .repo
        .record_event(job_id, "WARN", None, &format!("Cancelled by its journalist {} (#{})", user.email, user.id));
    let _ = state.repo.log_audit(
        "WARN",
        "USER",
        &format!("Job #{job_id}: cancelled — by {} (#{})", user.email, user.id),
    );
    state.broadcast_event("job_updated");
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `GET /api/user/prefs`.
pub async fn api_get_prefs(RequireUser(user): RequireUser, State(state): State<AppState>) -> JsonResult {
    let on = state
        .repo
        .get_notify_on_completion(user.id)
        .map_err(internal_error("Could not read your settings."))?
        .unwrap_or(false);
    Ok(Json(serde_json::json!({ "notify_on_completion": on })))
}

#[derive(Deserialize)]
pub struct PrefsPayload {
    notify_on_completion: bool,
}

/// `PUT /api/user/prefs`: stores the choice; sending is a later item.
pub async fn api_put_prefs(
    RequireUser(user): RequireUser,
    State(state): State<AppState>,
    Json(payload): Json<PrefsPayload>,
) -> JsonResult {
    state
        .repo
        .set_notify_on_completion(user.id, payload.notify_on_completion)
        .map_err(internal_error("Could not save your settings."))?;
    Ok(Json(serde_json::json!({ "notify_on_completion": payload.notify_on_completion })))
}

/// `GET /api/jobs/mine`: the caller's jobs without anything that describes the
/// server (file locations, lease holder, tool output, internal reports).
pub async fn api_get_my_jobs(RequireUser(user): RequireUser, State(state): State<AppState>) -> JsonResult {
    let jobs = state
        .repo
        .get_user_jobs(user.id)
        .map_err(internal_error("Could not read your jobs."))?;
    let jobs: Vec<serde_json::Value> = jobs.iter().map(job_for_user).collect();
    Ok(Json(serde_json::json!({ "jobs": jobs })))
}

/// Keys that say something about the server rather than about the video.
const SERVER_ONLY_KEYS: &[&str] = &[
    "file_path",
    "source_path",
    "lease_owner",
    "error_message",
    "compliance_json",
    "candidates_json",
    "stage_timings_json",
    "email_message_id",
    "url_normalized",
];

fn job_for_user(job: &omni_core::models::Job) -> serde_json::Value {
    let mut v = serde_json::to_value(job).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = v.as_object_mut() {
        for key in SERVER_ONLY_KEYS {
            obj.remove(*key);
        }
        let delivered = job.file_path.as_deref().and_then(file_name_only);
        obj.insert(
            "delivered_name".into(),
            delivered.map_or(serde_json::Value::Null, serde_json::Value::from),
        );
    }
    v
}

/// The last path component, for a Windows or a UNC path alike.
fn file_name_only(path: &str) -> Option<&str> {
    path.rsplit(['\\', '/']).next().filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_failure_lists_what_was_done_and_what_was_not_attempted() {
        let done = vec![serde_json::json!({"job_id": 1, "slug": "1A_X_Y", "duplicate": false})];
        let (status, body) = submit_response(done, 4, true);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let list = body["results"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["job_id"], 1);
        assert!(list[1]["error"].is_string());
        assert_eq!(body["not_attempted"], 2);
        let (status, _) = submit_response(vec![], 1, false);
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn index_suffixes_run_a_to_t() {
        assert_eq!(index_suffix(0), 'A');
        assert_eq!(index_suffix(19), 'T');
    }

    #[test]
    fn file_name_only_drops_every_directory() {
        assert_eq!(file_name_only(r"\\dalet\ingest\1_X_Y.mxf"), Some("1_X_Y.mxf"));
        assert_eq!(file_name_only("C:/out/1_X_Y.mxf"), Some("1_X_Y.mxf"));
        assert_eq!(file_name_only(""), None);
    }
}
