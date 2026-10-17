//! The admin's Benchmarks card (plan P6.4): the stored 65-link corpus, the runs
//! made over it and each run's report; starting a run. Admin only.
//!
//! A run never touches the queue, the watchfolder or the archive. Nothing here
//! shows a full address: targets are named by number, domain and category, and a
//! result carries the host of the stream it found.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use omni_core::benchmark::{self, Mode, CORPUS};

use crate::auth::{internal_error, ApiError, RequireAdmin};
use crate::state::{AppState, BenchRequest, BenchStartError};

type JsonResult = Result<Json<Value>, ApiError>;

/// Targets run at once unless the admin says otherwise.
pub const DEFAULT_PARALLEL: u32 = 2;
/// The most that may run at once.
pub const MAX_PARALLEL: u32 = 4;
/// Runs the card lists.
const RUNS_LISTED: usize = 30;

pub async fn api_admin_benchmarks(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let mut targets = state.repo.benchmark_targets().map_err(internal_error("Could not read the benchmark targets."))?;
    if targets.is_empty() {
        // The corpus is stored by the daemon at start-up; a fresh database that has not seen it yet gets it now.
        state.repo.seed_benchmark_targets().map_err(internal_error("Could not store the benchmark targets."))?;
        targets = state.repo.benchmark_targets().map_err(internal_error("Could not read the benchmark targets."))?;
    }
    let runs = state.repo.benchmark_runs(RUNS_LISTED).map_err(internal_error("Could not read the benchmark runs."))?;
    let running_jobs = state.repo.running_job_ids().map_err(internal_error("Could not read the running jobs."))?.len();
    let runs: Vec<Value> = runs
        .iter()
        .map(|r| {
            let summary: Option<Value> = r.summary_json.as_deref().and_then(|s| serde_json::from_str(s).ok());
            let pick = |key: &str| summary.as_ref().and_then(|s| s.get(key)).cloned().unwrap_or(Value::Null);
            json!({
                "id": r.id,
                "mode": r.mode.as_str(),
                "state": r.state,
                "started_at": r.started_at.map(omni_core::timestamps::format),
                "finished_at": r.finished_at.map(omni_core::timestamps::format),
                "paused": r.paused,
                "params": serde_json::from_str::<Value>(&r.params_json).unwrap_or(Value::Null),
                "targets_run": pick("targets_run"),
                "extract_pct": pick("extract_pct"),
                "delivery_pct": pick("delivery_pct"),
                "pass": pick("pass"),
            })
        })
        .collect();
    // No address: the card names a target by number, domain and category.
    let targets: Vec<Value> = targets
        .iter()
        .map(|t| {
            json!({
                "idx": t.idx,
                "domain": t.domain,
                "category": t.category,
                "friction": t.friction,
                "expected": t.expected.as_str(),
                "note": t.note,
            })
        })
        .collect();
    Ok(Json(json!({
        "runs": runs,
        "targets": targets,
        "can_start": state.benchmark_starter.is_some(),
        "running_jobs": running_jobs,
        "default_parallel": DEFAULT_PARALLEL,
        "max_parallel": MAX_PARALLEL,
        "corpus_size": CORPUS.len(),
        "thresholds": {
            "extract_min_pct": benchmark::EXTRACT_MIN_PCT,
            "delivery_min_pct": benchmark::DELIVERY_MIN_PCT,
            "max_compliance_fails": benchmark::MAX_COMPLIANCE_FAILS,
            "mean_extract_max_s": benchmark::MEAN_EXTRACT_MAX_MS / 1000.0,
        },
    })))
}

#[derive(Deserialize)]
pub struct RunPayload {
    mode: String,
    /// `1-65`, `1-5,9`; empty means all of them.
    #[serde(default)]
    targets: String,
    #[serde(default)]
    parallel: Option<u32>,
    #[serde(default)]
    force: bool,
}

pub async fn api_admin_benchmark_run(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<RunPayload>,
) -> JsonResult {
    let mode = Mode::parse(payload.mode.trim()).ok_or_else(|| ApiError::bad_request("The mode is extract or full."))?;
    let spec = if payload.targets.trim().is_empty() { format!("1-{}", CORPUS.len()) } else { payload.targets.clone() };
    let targets = benchmark::parse_targets(&spec, CORPUS.len() as i64).map_err(ApiError::bad_request)?;
    let parallel = payload.parallel.unwrap_or(DEFAULT_PARALLEL);
    if parallel == 0 || parallel > MAX_PARALLEL {
        return Err(ApiError::bad_request(format!("Run 1 to {MAX_PARALLEL} targets at once.")));
    }
    let Some(starter) = state.benchmark_starter.as_ref() else {
        return Err(ApiError::new(StatusCode::CONFLICT, "UNAVAILABLE", "The benchmark is not available here."));
    };
    let notify_state = state.clone();
    let request = BenchRequest {
        mode,
        targets: targets.clone(),
        parallel,
        force: payload.force,
        notify: Arc::new(move || notify_state.broadcast_event("benchmark")),
    };
    match starter.start(request) {
        Ok(run_id) => {
            let _ = state.repo.log_audit(
                "INFO",
                "ADMIN",
                &format!(
                    "{} started benchmark run {run_id} ({}, {} targets, {} at once{})",
                    admin.email,
                    mode.as_str(),
                    targets.len(),
                    parallel,
                    if payload.force { ", forced" } else { "" }
                ),
            );
            state.broadcast_event("benchmark");
            Ok(Json(json!({ "run_id": run_id, "mode": mode.as_str(), "targets": targets.len(), "parallel": parallel })))
        }
        Err(BenchStartError::Refused(why)) => Err(ApiError::new(StatusCode::CONFLICT, "BENCHMARK_REFUSED", why)),
        Err(BenchStartError::Failed) => Err(ApiError::internal("The benchmark could not be started.")),
    }
}

pub async fn api_admin_benchmark_report(
    RequireAdmin(_): RequireAdmin,
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    let report = benchmark::build_report(&state.repo, id).map_err(internal_error("Could not build the benchmark report."))?;
    match report {
        Some(r) => Ok(Json(serde_json::to_value(r).map_err(|e| {
            tracing::error!(error = ?e, "benchmark report");
            ApiError::internal("Could not build the benchmark report.")
        })?)),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "NOT_FOUND", "There is no such benchmark run.")),
    }
}
