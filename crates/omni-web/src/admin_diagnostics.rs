//! «Διαγνωστικό πακέτο» (P13.8): one admin button that downloads a zip for
//! support. What goes in, and what never does, is `omni_core::diagnostics`.
//!
//! The zip is written under the install's temp folder
//! (`temp\diagnostics\omni-diagnostics-<time>.zip`), the newest three are
//! kept, and one package is made at a time.
//!
//! The panel links here directly (no script): a package downloads, anything
//! else (busy, signed out, an error) is a short page that says what happened
//! and links back, never a JSON body the browser would save as a `.zip`.

use std::path::PathBuf;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};

use omni_core::dependencies::DependencyManager;
use omni_core::diagnostics::{self, Limits, Sources};
use omni_core::paths::AppPaths;

use crate::auth::{ApiError, RequireAdmin};
use crate::state::AppState;

/// Packages kept in `temp\diagnostics`.
const KEEP: usize = 3;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// An error as a page a person reads, with its status.
fn error_page(e: ApiError) -> Response {
    let body = format!(
        "<!doctype html><html lang=\"el\"><head><meta charset=\"utf-8\"><title>Διαγνωστικό πακέτο</title></head>\
         <body style=\"font-family:sans-serif;max-width:40em;margin:3em auto\"><h1>Διαγνωστικό πακέτο</h1>\
         <p>{}</p><p style=\"color:#666\">{}</p><p><a href=\"/admin\">Επιστροφή στη διαχείριση</a></p></body></html>",
        escape(&e.message),
        e.code
    );
    (e.status, [(header::CACHE_CONTROL, "no-store")], Html(body)).into_response()
}

/// The install the daemon runs from (never the working directory).
fn install(state: &AppState) -> AppPaths {
    match &state.delivery {
        Some(live) => live.install.clone(),
        None => {
            let root = state.config_path.parent().map(PathBuf::from).unwrap_or_default();
            let name = state.config_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "config.json".into());
            AppPaths::with_root(root, &name)
        }
    }
}

/// Every secret value the daemon knows, to be written as `***`.
async fn secret_values(state: &AppState) -> Vec<String> {
    let mut values = state.secrets.all_values();
    values.extend(state.repo.cookie_values());
    let cfg = state.config.read().await;
    // Values from the environment (OMNI_GRAPH_CLIENT_SECRET, OMNI_LLM_API_KEY) live only here.
    values.extend([cfg.graph.client_secret.clone(), cfg.llm.api_key.clone()].into_iter().filter(|v| v.len() >= 6));
    // Longest first (P13.8b): a value that begins a longer one must not leave its tail.
    values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    values.dedup();
    values
}

pub async fn api_admin_diagnostics(admin: Result<RequireAdmin, ApiError>, State(state): State<AppState>) -> Response {
    let made = match admin {
        Ok(RequireAdmin(admin)) => make(admin, state).await,
        Err(e) => Err(e),
    };
    made.unwrap_or_else(error_page)
}

async fn make(admin: omni_core::models::User, state: AppState) -> Result<Response, ApiError> {
    // Owned, and moved into the blocking writer below: a browser that goes
    // away does not free it while the file is still being written.
    let Ok(one) = state.diagnostics_lock.clone().try_lock_owned() else {
        return Err(ApiError::new(StatusCode::CONFLICT, "BUSY", "Ένα διαγνωστικό πακέτο ετοιμάζεται ήδη· περιμένετε να κατέβει και δοκιμάστε ξανά."));
    };
    let paths = install(&state);
    let out_dir = state.temp_dir.clone().unwrap_or_else(|| paths.temp.clone()).join("diagnostics");
    std::fs::create_dir_all(&out_dir).map_err(|e| ApiError::internal(format!("Could not create {}: {e}", out_dir.display())))?;

    let bin = crate::admin_tools::tool_bin_dir(&state).await;
    let tools: Vec<serde_json::Value> = DependencyManager::new(bin)
        .tool_statuses(&state.tool_installer, &state.tool_overrides)
        .await
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name, "installed": t.installed, "version": t.version,
                "pinned_version": t.pinned_version, "matches_pin": t.matches_pin, "source": t.source,
            })
        })
        .collect();
    let limits = Limits::default();
    let mut failed = Vec::new();
    for job in state.repo.recent_failed_jobs(limits.failed_jobs).map_err(|e| ApiError::internal(format!("Could not read the failed jobs: {e:#}")))? {
        let events = state.repo.recent_job_events(job.id, 100).unwrap_or_default();
        failed.push((job, events));
    }
    let now = chrono::Utc::now();
    let src = Sources {
        logs_dir: paths.logs.clone(),
        config_path: state.config_path.clone(),
        health: serde_json::to_value(state.health.all()).unwrap_or_default(),
        tools: serde_json::Value::Array(tools),
        failed,
        secrets: secret_values(&state).await,
        version: env!("CARGO_PKG_VERSION").to_string(),
        now,
    };
    let name = format!("omni-diagnostics-{}.zip", now.format("%Y%m%d-%H%M%S"));
    let path = out_dir.join(&name);
    let (dir, file) = (out_dir.clone(), path.clone());
    let (written, bytes) = tokio::task::spawn_blocking(move || {
        // Held until the file is written and read back, whatever the client does.
        let _one = one;
        let w = diagnostics::write_zip(&file, &src, limits).and_then(|w| Ok((w, std::fs::read(&file)?)));
        if w.is_err() {
            let _ = std::fs::remove_file(&file);
        }
        diagnostics::prune(&dir, KEEP);
        w
    })
    .await
    .map_err(|_| ApiError::internal("The diagnostics package could not be made."))?
    .map_err(|e| {
        tracing::error!(error = %format!("{e:#}"), "The diagnostics package could not be made");
        ApiError::internal("The diagnostics package could not be made; see the service log.")
    })?;
    let _ = state.repo.log_audit(
        "INFO",
        "ADMIN",
        &format!("Diagnostics package {name} made by {} ({} log file(s), {} failed job(s))", admin.email, written.logs.len(), written.failed_jobs),
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
}
