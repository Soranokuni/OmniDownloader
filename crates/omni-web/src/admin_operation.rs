//! The Settings section's «Λειτουργία» form (P13.5): repair mode, how many
//! jobs run at once, how often the mailbox is read, Graph write access and
//! folders, yt-dlp pacing and the retry windows. Admin only.
//!
//! Every value is checked against the range the daemon reads it in; a value
//! outside it is refused with the key, the value and the range, never
//! clamped silently. What the running daemon reads live (the late strip, the
//! Graph settings) applies at once; the rest from the next start, and the
//! answer says which.

use axum::extract::State;
use axum::Json;
use serde::Deserialize;

use omni_core::config::{AppConfig, RepairMode};

use crate::auth::{internal_error, ApiError, RequireAdmin};
use crate::state::AppState;

type JsonResult = Result<Json<serde_json::Value>, ApiError>;

/// The form's values as the panel shows them.
pub fn operation_json(cfg: &AppConfig) -> serde_json::Value {
    serde_json::json!({
        "repair_mode": cfg.repair.mode,
        "max_concurrent_downloads": cfg.max_concurrent_downloads,
        "max_concurrent_transcodes": cfg.max_concurrent_transcodes,
        "mail_poll_interval_secs": cfg.mail.poll_interval_secs,
        "graph_write_access": cfg.graph.write_access,
        "graph_processed_folder": cfg.graph.processed_folder,
        "graph_failed_folder": cfg.graph.failed_folder,
        "pacing_per_host_concurrency": cfg.pacing.per_host_concurrency,
        "pacing_min_gap_secs": cfg.pacing.min_gap_secs,
        "retry_max_age_mins": cfg.retry.max_age_mins,
        "retry_late_after_mins": cfg.retry.late_after_mins,
        "retry_late_after_urgent_mins": cfg.retry.late_after_urgent_mins,
        "retry_second_chance_window_mins": cfg.retry.second_chance_window_mins,
    })
}

#[derive(Deserialize, Default)]
pub struct OperationPayload {
    repair_mode: Option<RepairMode>,
    max_concurrent_downloads: Option<i64>,
    max_concurrent_transcodes: Option<i64>,
    mail_poll_interval_secs: Option<i64>,
    graph_write_access: Option<bool>,
    graph_processed_folder: Option<String>,
    graph_failed_folder: Option<String>,
    pacing_per_host_concurrency: Option<i64>,
    pacing_min_gap_secs: Option<i64>,
    retry_max_age_mins: Option<i64>,
    retry_late_after_mins: Option<i64>,
    retry_late_after_urgent_mins: Option<i64>,
    retry_second_chance_window_mins: Option<i64>,
}

/// `value` if it is in `lo..=hi`, else the refusal naming key, value and range.
fn ranged(key: &str, value: i64, lo: i64, hi: i64) -> Result<i64, ApiError> {
    if (lo..=hi).contains(&value) {
        Ok(value)
    } else {
        Err(ApiError::bad_request(format!("{key} = {value} is outside {lo}..{hi}. Enter a value from {lo} to {hi}.")))
    }
}

/// A Graph mail folder path: `Omni/Processed`. Not blank, at most 120 characters.
fn folder(key: &str, raw: &str) -> Result<String, ApiError> {
    let f = raw.trim();
    if f.is_empty() || f.chars().count() > 120 || f.chars().any(char::is_control) {
        return Err(ApiError::bad_request(format!(
            "{key}: a mailbox folder path such as Omni/Processed is needed (1 to 120 characters)."
        )));
    }
    Ok(f.to_string())
}

/// Check `p` against `cfg` and return the new settings, or the first refusal.
pub fn apply(cfg: &AppConfig, p: &OperationPayload) -> Result<AppConfig, ApiError> {
    let mut n = cfg.clone();
    if let Some(m) = p.repair_mode {
        n.repair.mode = m;
    }
    if let Some(v) = p.max_concurrent_downloads {
        n.max_concurrent_downloads = ranged("max_concurrent_downloads", v, 1, 10)? as usize;
    }
    if let Some(v) = p.max_concurrent_transcodes {
        n.max_concurrent_transcodes = ranged("max_concurrent_transcodes", v, 1, 8)? as usize;
    }
    if let Some(v) = p.mail_poll_interval_secs {
        n.mail.poll_interval_secs = ranged("mail.poll_interval_secs", v, 10, 3600)? as u64;
    }
    if let Some(v) = p.graph_write_access {
        n.graph.write_access = v;
    }
    if let Some(f) = &p.graph_processed_folder {
        n.graph.processed_folder = folder("graph.processed_folder", f)?;
    }
    if let Some(f) = &p.graph_failed_folder {
        n.graph.failed_folder = folder("graph.failed_folder", f)?;
    }
    if let Some(v) = p.pacing_per_host_concurrency {
        n.pacing.per_host_concurrency = ranged("pacing.per_host_concurrency", v, 1, 4)?;
    }
    if let Some(v) = p.pacing_min_gap_secs {
        n.pacing.min_gap_secs = ranged("pacing.min_gap_secs", v, 0, 120)?;
    }
    if let Some(v) = p.retry_max_age_mins {
        n.retry.max_age_mins = ranged("retry.max_age_mins", v, 30, 360)?;
    }
    if let Some(v) = p.retry_late_after_mins {
        n.retry.late_after_mins = ranged("retry.late_after_mins", v, 1, 120)?;
    }
    if let Some(v) = p.retry_late_after_urgent_mins {
        n.retry.late_after_urgent_mins = ranged("retry.late_after_urgent_mins", v, 1, 120)?;
    }
    if let Some(v) = p.retry_second_chance_window_mins {
        n.retry.second_chance_window_mins = ranged("retry.second_chance_window_mins", v, 30, 360)?;
    }
    Ok(n)
}

/// Whether going from `old` to `new` needs a restart: everything here except
/// the late strip (read per request) and the Graph settings (the watcher's
/// source is rebuilt).
pub fn needs_restart(old: &AppConfig, new: &AppConfig) -> bool {
    old.repair.mode != new.repair.mode
        || old.max_concurrent_downloads != new.max_concurrent_downloads
        || old.max_concurrent_transcodes != new.max_concurrent_transcodes
        || old.mail.poll_interval_secs != new.mail.poll_interval_secs
        || old.pacing.per_host_concurrency != new.pacing.per_host_concurrency
        || old.pacing.min_gap_secs != new.pacing.min_gap_secs
        || old.retry.max_age_mins != new.retry.max_age_mins
        || old.retry.second_chance_window_mins != new.retry.second_chance_window_mins
}

pub async fn api_admin_save_operation(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(p): Json<OperationPayload>,
) -> JsonResult {
    let mut cfg = state.config.write().await;
    let next = apply(&cfg, &p)?;
    let restart = needs_restart(&cfg, &next);
    let graph_changed = next.graph.write_access != cfg.graph.write_access
        || next.graph.processed_folder != cfg.graph.processed_folder
        || next.graph.failed_folder != cfg.graph.failed_folder;
    next.save_to_file(&state.config_path).map_err(internal_error("Could not save the configuration."))?;
    // Under the config lock, as the Mailbox card: saves reach the watcher in order.
    let graph_live = !graph_changed || state.apply_mail_settings(&next.graph);
    *cfg = next;
    let doc = operation_json(&cfg);
    drop(cfg);
    let _ = state.repo.log_audit("WARN", "ADMIN", &format!("Operation settings changed by {} (settings)", admin.email));
    Ok(Json(serde_json::json!({
        "status": "ok",
        "restart_required": restart || !graph_live,
        "operation": doc,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(v: serde_json::Value) -> OperationPayload {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn values_in_range_are_taken_and_out_of_range_ones_are_named() {
        let cfg = AppConfig::default();
        let n = apply(&cfg, &payload(serde_json::json!({
            "repair_mode": "shadow", "max_concurrent_downloads": 4, "mail_poll_interval_secs": 60,
            "graph_write_access": true, "graph_processed_folder": " Omni/Done ", "pacing_min_gap_secs": 0,
            "retry_late_after_mins": 15,
        })))
        .unwrap();
        assert_eq!(n.repair.mode, RepairMode::Shadow);
        assert_eq!((n.max_concurrent_downloads, n.mail.poll_interval_secs, n.pacing.min_gap_secs), (4, 60, 0));
        assert_eq!((n.graph.write_access, n.graph.processed_folder.as_str()), (true, "Omni/Done"));
        assert_eq!(n.max_concurrent_transcodes, cfg.max_concurrent_transcodes, "what is not sent is kept");
        assert!(needs_restart(&cfg, &n));

        for (doc, key) in [
            (serde_json::json!({ "max_concurrent_downloads": 0 }), "max_concurrent_downloads = 0 is outside 1..10"),
            (serde_json::json!({ "mail_poll_interval_secs": 5 }), "mail.poll_interval_secs = 5"),
            (serde_json::json!({ "retry_max_age_mins": 600 }), "retry.max_age_mins = 600"),
            (serde_json::json!({ "pacing_per_host_concurrency": 9 }), "pacing.per_host_concurrency = 9"),
            (serde_json::json!({ "graph_failed_folder": "  " }), "graph.failed_folder"),
        ] {
            let e = apply(&cfg, &payload(doc)).err().expect("refused");
            assert!(e.message.contains(key), "{}", e.message);
        }
    }

    #[test]
    fn only_the_late_strip_and_graph_need_no_restart() {
        let cfg = AppConfig::default();
        let live = apply(&cfg, &payload(serde_json::json!({ "retry_late_after_mins": 20, "graph_write_access": true }))).unwrap();
        assert!(!needs_restart(&cfg, &live));
    }
}
