//! «Διαγνωστικό πακέτο» (P13.8): one admin button that downloads a zip for
//! support. What goes in, and what never does, is `omni_core::diagnostics`.
//!
//! The zip is written under the install's temp folder
//! (`temp\diagnostics\omni-diagnostics-<time>.zip`), the newest three are
//! kept, and one package is made at a time.

use std::path::PathBuf;
use std::sync::OnceLock;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use omni_core::dependencies::DependencyManager;
use omni_core::diagnostics::{self, Limits, Sources};
use omni_core::paths::AppPaths;

use crate::auth::{ApiError, RequireAdmin};
use crate::state::AppState;

/// Packages kept in `temp\diagnostics`.
const KEEP: usize = 3;

fn one_at_a_time() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
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
    values.sort();
    values.dedup();
    values
}

pub async fn api_admin_diagnostics(RequireAdmin(admin): RequireAdmin, State(state): State<AppState>) -> Result<Response, ApiError> {
    let Ok(_one) = one_at_a_time().try_lock() else {
        return Err(ApiError::new(StatusCode::CONFLICT, "BUSY", "A diagnostics package is being made; wait for it to download."));
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
    let written = tokio::task::spawn_blocking(move || {
        let w = diagnostics::write_zip(&file, &src, limits);
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
    let bytes = tokio::fs::read(&path).await.map_err(|e| ApiError::internal(format!("Could not read the package: {e}")))?;
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
