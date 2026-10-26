//! The admin's toolchain card (plan P9.3): which external tools are installed,
//! whether ffmpeg, ffprobe and bmxtranswrap are the pinned build, and the
//! «Install» / «Repair» buttons. Admin only.
//!
//! ffmpeg, ffprobe and bmxtranswrap are fetched only at the build pinned in
//! `omni_core::toolpins`, checked against its SHA-256, and never updated by a
//! schedule. An install runs in the background; one at a time.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::path::PathBuf;

use omni_core::dependencies::{DependencyManager, InstallResult, SWAP_DRAIN};
use omni_core::toolpins;

use crate::auth::{ApiError, RequireAdmin};
use crate::state::AppState;

type JsonResult = Result<Json<Value>, ApiError>;

/// Tools the install route accepts.
const INSTALLABLE: &[&str] = &["ffmpeg", "ffprobe", "bmxtranswrap", "deno", "yt-dlp"];

/// The tool an install of `name` is recorded under: ffprobe comes with ffmpeg.
fn group_of(name: &str) -> String {
    toolpins::pin_for(name).map(|p| p.tool.to_string()).unwrap_or_else(|| name.to_string())
}

/// How long the synchronous legacy routes wait for running jobs.
pub const LEGACY_DRAIN: std::time::Duration = std::time::Duration::from_secs(60);

/// The right to swap yt-dlp or Deno: the canary run lock, then the gate's swap
/// lock (the nightly task takes them in the same order). Dropping it reopens
/// the gate and frees both, on every path.
pub struct SwapLocks {
    pub swap: omni_core::update_gate::SwapGuard,
    _canary: Option<tokio::sync::OwnedMutexGuard<()>>,
}

pub async fn lock_swaps(state: &AppState) -> SwapLocks {
    let canary = match &state.canary_lock {
        Some(lock) => Some(lock.clone().lock_owned().await),
        None => None,
    };
    SwapLocks { swap: state.update_gate.lock_swaps().await, _canary: canary }
}

/// `bin/`, anchored to the install (never the working directory) in the daemon.
pub async fn tool_bin_dir(state: &AppState) -> PathBuf {
    bin_dir(state).await
}

async fn bin_dir(state: &AppState) -> PathBuf {
    if let Some(b) = &state.bin_dir {
        return b.clone();
    }
    let cfg = state.config.read().await;
    state.config_path.parent().unwrap_or_else(|| std::path::Path::new(".")).join(&cfg.bin_dir)
}

pub async fn api_admin_tools(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let mgr = DependencyManager::new(bin_dir(&state).await);
    let rows = mgr.tool_statuses(&state.tool_installer, &state.tool_overrides).await;
    let running = state.tool_installer.running();
    let tools: Vec<Value> = rows
        .iter()
        .map(|t| {
            let group = group_of(&t.name);
            let running_here = running.as_ref().filter(|r| r.tool == group).map(|r| {
                json!({ "bytes_done": r.bytes_done, "bytes_total": r.bytes_total, "stage": r.stage })
            });
            let last = state.tool_installer.last_result(&group).map(|r| {
                json!({ "ok": r.ok, "message": r.message, "needs_restart": r.needs_restart })
            });
            json!({
                "name": t.name,
                "path": t.path,
                "installed": t.installed,
                "version": t.version,
                "pinned_version": t.pinned_version,
                "matches_pin": t.matches_pin,
                "action": t.action,
                "source": t.source,
                "note": t.note,
                "running": running_here,
                "last_result": last,
            })
        })
        .collect();
    Ok(Json(json!({ "tools": tools, "running_tool": running.map(|r| r.tool) })))
}

pub async fn api_admin_tool_install(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !INSTALLABLE.contains(&name.as_str()) {
        return Err(ApiError::bad_request(format!("Unknown tool `{}`.", name.chars().take(40).collect::<String>())));
    }
    let group = group_of(&name);
    let guard = match state.tool_installer.try_begin(&group) {
        Ok(g) => g,
        Err(running) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "BUSY",
                format!("{running} is being installed; wait until it has finished."),
            ));
        }
    };
    let _ = state.repo.log_audit("INFO", "ADMIN", &format!("{} started the install of {group}", admin.email));
    let dir = bin_dir(&state).await;
    let channel = state.config.read().await.ytdl_channel.clone();
    let task_state = state.clone();
    let who = admin.email.clone();
    let task_group = group.clone();
    tokio::spawn(async move {
        let outcome = run_install(&task_state, &dir, &task_group, &channel).await;
        let needs_restart = outcome.is_ok()
            && match task_group.as_str() {
                "ffmpeg" => ["ffmpeg", "ffprobe"].iter().any(|t| task_state.tools_missing_at_start.iter().any(|m| m == t)),
                "deno" => false,
                other => task_state.tools_missing_at_start.iter().any(|m| m == other),
            };
        let result = match outcome {
            Ok(message) => {
                let message = if needs_restart {
                    format!("{message} Restart the service to use it.")
                } else {
                    message
                };
                let _ = task_state.repo.log_audit("INFO", "ADMIN", &format!("{who} installed {task_group}: {message}"));
                InstallResult { ok: true, message, needs_restart }
            }
            Err(e) => {
                let message = format!("{e:#}");
                tracing::error!(tool = %task_group, error = %message, "tool install failed");
                let _ = task_state.repo.log_audit("WARN", "ADMIN", &format!("{who}: install of {task_group} failed: {message}"));
                InstallResult { ok: false, message, needs_restart: false }
            }
        };
        guard.finish(result);
        // The pinned-tools health line follows the files, whatever the install said.
        let (mgr, st, ov) = (DependencyManager::new(&dir), task_state.tool_installer.clone(), task_state.tool_overrides.clone());
        if let Ok(check) = tokio::task::spawn_blocking(move || mgr.pinned_tools_check(&st, &ov)).await {
            task_state.health.set(omni_core::health::checks::PINNED_TOOLS, check);
        }
        task_state.broadcast_event("tools");
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "started", "tool": group }))))
}

/// Do one install and say what happened. Never leaves the gate paused.
async fn run_install(state: &AppState, dir: &std::path::Path, group: &str, channel: &str) -> anyhow::Result<String> {
    let mgr = DependencyManager::new(dir);
    let st = &state.tool_installer;
    let gate = &state.update_gate;
    let overridden = |tool: &str| state.tool_overrides.contains_key(tool);
    let note = match group {
        "deno" => {
            // Download first; pause only around the rename.
            let mut locks = lock_swaps(state).await;
            st.set_stage("downloading");
            let version = mgr.stage_deno().await?;
            st.set_stage("waiting for jobs");
            if !locks.swap.drain(SWAP_DRAIN).await {
                mgr.discard_staged_deno();
                anyhow::bail!("jobs still running; try again");
            }
            st.set_stage("installing");
            if let Err(e) = mgr.apply_staged_deno(&version) {
                mgr.discard_staged_deno();
                return Err(e);
            }
            format!("Deno {version} installed.")
        }
        "yt-dlp" => {
            let mut locks = lock_swaps(state).await;
            st.set_stage("downloading");
            mgr.stage_ytdl(channel).await?;
            st.set_stage("waiting for jobs");
            if !locks.swap.drain(SWAP_DRAIN).await {
                mgr.discard_staged_ytdl();
                anyhow::bail!("jobs still running; try again");
            }
            st.set_stage("installing");
            if let Err(e) = mgr.apply_staged_ytdl() {
                mgr.discard_staged_ytdl();
                return Err(e);
            }
            "yt-dlp installed; the previous build is kept for a rollback.".to_string()
        }
        other => {
            let pin = toolpins::pin_for(other).ok_or_else(|| anyhow::anyhow!("no pinned build for {other}"))?;
            mgr.install_pinned(pin, &*state.tool_fetcher, gate, SWAP_DRAIN, st).await?.message
        }
    };
    // The check after the swap: what the installed tool now reports.
    let installed = mgr.tool_statuses(st, &state.tool_overrides).await;
    let shown: Vec<&str> = if group == "ffmpeg" { vec!["ffmpeg", "ffprobe"] } else { vec![group] };
    let versions: Vec<String> = installed
        .iter()
        .filter(|t| shown.contains(&t.name.as_str()))
        .map(|t| format!("{} {}", t.name, t.version.as_deref().unwrap_or("does not report a version")))
        .collect();
    let mut out = format!("{note} Now: {}.", versions.join(", "));
    if shown.iter().any(|t| overridden(t)) {
        out.push_str(" config.json points to another file for this tool; that file is what runs.");
    }
    Ok(out)
}
