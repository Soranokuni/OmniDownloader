//! `GET /metrics`, the Prometheus scrape (plan P6.5).
//!
//! Off by default: with `metrics.enabled` false the route is a 404 like any
//! unknown path and nothing is computed. On, it answers a client in
//! `metrics.allow_networks` or an Admin session; `security.mcr_open_networks`
//! and MCR or user sessions do not count.
//!
//! The job counts, disk space, workers, mail poll age and health are read at
//! scrape time and written here as text; the stage histogram and the finished
//! counter come from the recorder, which the repository feeds. Labels are
//! fixed small sets (status, stage, check, volume), and no value carries a URL,
//! slug, name, address or path.

use std::fmt::Write as _;
use std::path::PathBuf;

use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::auth::{client_ip, internal_error, resolve_principal, ApiError};
use crate::state::AppState;

/// Prometheus text exposition format.
const CONTENT_TYPE_TEXT: &str = "text/plain; version=0.0.4";

pub async fn api_metrics(State(state): State<AppState>, req: Request) -> Response {
    let (allow, enabled, trusted, trust_header) = {
        let cfg = state.config.read().await;
        (
            cfg.metrics.networks(),
            cfg.metrics.enabled,
            cfg.security.trusted_proxy_networks(),
            cfg.security.trust_proxy_header,
        )
    };
    // Disabled is indistinguishable from a route that does not exist.
    let Some(handle) = state.metrics.clone().filter(|_| enabled) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let (parts, _) = req.into_parts();
    let from_allowed_network = client_ip(&parts, &trusted, trust_header).is_some_and(|ip| allow.contains(ip));
    if !from_allowed_network {
        let admin = matches!(resolve_principal(&parts, &state).await, Some(p) if p.is_admin());
        if !admin {
            return ApiError::forbidden().into_response();
        }
    }

    let jobs = match state.repo.job_counts_by_status() {
        Ok(j) => j,
        Err(e) => return internal_error("metrics")(e).into_response(),
    };
    let sources = state.metrics_sources.clone();
    // Disk space asks the volume (a share can be slow), so not on the async threads.
    let disks = tokio::task::spawn_blocking(move || disk_free(sources.as_deref()))
        .await
        .unwrap_or_default();

    let mut out = handle.render();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    gauge_header(&mut out, "omni_jobs", "Jobs per status.");
    for (status, n) in jobs {
        let _ = writeln!(out, "omni_jobs{{status=\"{}\"}} {n}", status.as_str());
    }
    if let Some(at) = state.health.last_mail_poll() {
        gauge_header(&mut out, "omni_mail_last_poll_age_seconds", "Seconds since the last successful mail poll.");
        let age = (chrono::Utc::now() - at).num_milliseconds().max(0) as f64 / 1000.0;
        let _ = writeln!(out, "omni_mail_last_poll_age_seconds {age:.3}");
    }
    if !disks.is_empty() {
        gauge_header(&mut out, "omni_disk_free_bytes", "Free bytes on the volume of temp, the watchfolder and the archive.");
        for (volume, free) in disks {
            let _ = writeln!(out, "omni_disk_free_bytes{{volume=\"{volume}\"}} {free}");
        }
    }
    gauge_header(&mut out, "omni_workers_busy", "Job runs in progress.");
    let _ = writeln!(out, "omni_workers_busy {}", state.busy_jobs.len());
    if let Some(s) = &state.metrics_sources {
        gauge_header(&mut out, "omni_workers_max", "Configured number of workers.");
        let _ = writeln!(out, "omni_workers_max {}", s.workers_max);
    }
    gauge_header(&mut out, "omni_health", "Subsystem health: 0 ok, 1 degraded, 2 down.");
    for (name, check) in state.health.all() {
        // Check names are fixed identifiers; anything else would not be a legal label value here.
        if name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            let v = match check.state {
                omni_core::health::Health::Ok => 0,
                omni_core::health::Health::Degraded => 1,
                omni_core::health::Health::Down => 2,
            };
            let _ = writeln!(out, "omni_health{{check=\"{name}\"}} {v}");
        }
    }

    ([(CONTENT_TYPE, CONTENT_TYPE_TEXT)], out).into_response()
}

fn gauge_header(out: &mut String, name: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
}

/// `(volume, free bytes)` for each volume that can be read.
fn disk_free(sources: Option<&crate::state::MetricsSources>) -> Vec<(&'static str, u64)> {
    let Some(s) = sources else { return Vec::new() };
    let mut volumes: Vec<(&'static str, &PathBuf)> = vec![("temp", &s.temp), ("watchfolder", &s.watchfolder)];
    if let Some(a) = &s.archive {
        volumes.push(("archive", a));
    }
    volumes
        .into_iter()
        .filter_map(|(name, path)| omni_core::selftest::disk_space(path).map(|(free, _)| (name, free)))
        .collect()
}
