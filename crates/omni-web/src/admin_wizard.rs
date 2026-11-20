//! The setup wizard's last steps (P13.4): the journalists import and
//! «Δοκιμαστική λήψη», a known-good link run through every stage into the
//! watchfolder. Admin only.
//!
//! The test download is an ordinary job, made through `Repository::enqueue`
//! and run by the worker pool like any other; the wizard only watches it.
//! Setup counts as complete only after one has been delivered or the
//! administrator has skipped it on purpose (`api_admin_complete_setup`).

use axum::extract::{Path as AxumPath, State};
use axum::Json;
use serde::Deserialize;

use omni_core::models::{Job, JobStatus, NewJob};

use crate::admin_roster;
use crate::auth::{internal_error, ApiError, RequireAdmin};
use crate::state::AppState;

type JsonResult = Result<Json<serde_json::Value>, ApiError>;

/// The test job's name, and so its file's in the watchfolder:
/// `1_MCR_OMNIINGEST-TEST.mxf` (`_2`, `_3`… when one is already there).
pub const TEST_SLUG: &str = "1_MCR_OMNIINGEST-TEST";
const TEST_JOURNALIST: &str = "MCR";
const TEST_KEYWORD: &str = "OMNIINGEST-TEST";
const TEST_NOTE: &str = "Δοκιμαστική λήψη του οδηγού εγκατάστασης";

/// Whether `job` is a wizard test download.
pub fn is_test_job(job: &Job) -> bool {
    job.slug == TEST_SLUG && job.journalist == TEST_JOURNALIST
}

/// Whether `job` proves that a video reaches the watchfolder: a test job,
/// completed by the pipeline and delivered.
pub fn proves_delivery(job: &Job) -> bool {
    is_test_job(job) && job.status == JobStatus::Completed && job.delivered_at.is_some() && job.file_path.is_some()
}

#[derive(Deserialize)]
pub struct TestDownloadPayload {
    #[serde(default)]
    url: Option<String>,
}

/// Start the test download (or return the one still running). The link is
/// the one given, else `setup_test_url`; a changed link is saved as the new
/// `setup_test_url`.
pub async fn api_admin_start_test_download(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(p): Json<TestDownloadPayload>,
) -> JsonResult {
    let configured = state.config.read().await.setup_test_url.clone();
    let url = p.url.as_deref().map(str::trim).filter(|u| !u.is_empty()).unwrap_or(&configured).to_string();
    if !crate::routes::is_submittable_url(&url) {
        return Err(ApiError::bad_request("setup_test_url: a link that starts with http:// or https:// is needed."));
    }
    if url != configured {
        let mut cfg = state.config.write().await;
        let mut next = cfg.clone();
        next.setup_test_url = url.clone();
        next.save_to_file(&state.config_path).map_err(internal_error("Could not save the configuration."))?;
        *cfg = next;
    }
    let mut job = NewJob::new(url.clone(), TEST_SLUG, TEST_JOURNALIST);
    job.keyword = TEST_KEYWORD.into();
    job.submitted_by_user_id = Some(admin.id);
    job.notes = Some(TEST_NOTE.into());
    // No "recently delivered" match: a second test is a new test. One still
    // queued or running is returned instead of a second one.
    let enqueued = state.repo.enqueue(&job, 0).map_err(internal_error("The test download could not be queued."))?;
    let id = enqueued.job_id();
    if enqueued.is_new() {
        let _ = state.repo.log_audit("INFO", "ADMIN", &format!("Setup test download #{id} started by {}", admin.email));
        state.broadcast_job_event("job_created", id);
    } else {
        let existing = state.repo.get_job(id).map_err(internal_error("Could not read the job."))?;
        if !existing.as_ref().is_some_and(is_test_job) {
            return Err(ApiError::bad_request(format!(
                "This link is already job #{id} on the desk. Wait for it to finish, or use another test link."
            )));
        }
    }
    Ok(Json(serde_json::json!({ "job_id": id, "new": enqueued.is_new() })))
}

/// The test download's progress, for the wizard's live view.
pub async fn api_admin_test_download_status(
    RequireAdmin(_): RequireAdmin,
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    let job = state
        .repo
        .get_job(id)
        .map_err(internal_error("Could not read the job."))?
        .filter(is_test_job)
        .ok_or_else(ApiError::not_found)?;
    let events = state.repo.recent_job_events(id, 12).map_err(internal_error("Could not read the job."))?;
    let file_name = job
        .file_path
        .as_deref()
        .filter(|_| proves_delivery(&job))
        .and_then(|p| std::path::Path::new(p).file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let watchfolder = match &state.delivery {
        Some(live) => live.paths.watchfolder().to_string_lossy().into_owned(),
        None => {
            let cfg = state.config.read().await;
            cfg.resolve_path(&cfg.watchfolder_path).to_string_lossy().into_owned()
        }
    };
    Ok(Json(serde_json::json!({
        "job_id": job.id,
        "status": job.status.as_str(),
        "stage": job.stage.as_str(),
        "progress": job.progress,
        "error_code": job.error_code,
        "error_message": job.error_message,
        "delivered": proves_delivery(&job),
        "file_name": file_name,
        "watchfolder": watchfolder,
        "events": events.iter().map(|e| serde_json::json!({ "level": e.level, "stage": e.stage, "message": e.message })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct RosterPayload {
    text: String,
    /// Only say what an import would do.
    #[serde(default)]
    dry_run: bool,
}

/// Import journalists pasted from Excel or read from a CSV (P13.4). People
/// already on the list are left as they are; every line that cannot be
/// imported is named with its number and the reason. A copy of the list is
/// kept before anything is added.
pub async fn api_admin_import_journalists(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(p): Json<RosterPayload>,
) -> JsonResult {
    if p.text.len() > 512 * 1024 {
        return Err(ApiError::bad_request("The list is larger than 512 KB; import it in parts."));
    }
    let token = |s: &str| crate::routes::sanitize_token(s, "");
    let (rows, problems) = admin_roster::parse(&p.text, &token);
    let existing: Vec<(String, Vec<String>)> = state
        .repo
        .list_journalists()
        .map_err(internal_error("Could not read the journalists."))?
        .into_iter()
        .map(|j| (j.surname, j.emails))
        .collect();
    let plan = admin_roster::plan(rows, problems, &existing);
    if !p.dry_run && !plan.add.is_empty() {
        state.repo.backup_taxonomy("before-import").map_err(internal_error("Could not keep a copy of the list; nothing was imported."))?;
        for row in &plan.add {
            state
                .repo
                .save_journalist(&row.surname, &row.full_name, &row.emails, 0)
                .map_err(internal_error("Could not save a journalist; the copy of the list made before the import is in data/backups/taxonomy."))?;
        }
        let _ = state.repo.log_audit("INFO", "ADMIN", &format!("{} journalist(s) imported by {} (setup)", plan.add.len(), admin.email));
    }
    Ok(Json(serde_json::json!({
        "imported": !p.dry_run,
        "add": plan.add,
        "existing": plan.existing,
        "problems": plan.problems,
    })))
}
