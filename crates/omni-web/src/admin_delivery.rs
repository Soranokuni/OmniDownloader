//! Everyday settings without a restart, and a restart button for the rest (P13.2).
//!
//! The watchfolder changes between jobs: [`apply_watchfolder`] hands the new
//! folder to [`omni_broadcast::pipeline::switch_watchfolder`], which stops new
//! leases on the update gate, waits for running jobs (as a yt-dlp swap does)
//! and only then switches; videos still waiting for the old folder go to
//! review. If jobs are still running after the drain the change stays
//! *pending* and is tried again in the background until it applies or a newer
//! save replaces it. The archive applies at once.
//!
//! Port, TLS, allowed host names and metrics on/off are read when the listener
//! starts: for those the panel offers «Επανεκκίνηση υπηρεσίας», which the
//! daemon carries out through a [`ServiceRestarter`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;

use omni_broadcast::pipeline::{switch_watchfolder, DeliveryPaths, WatchfolderSwitch};
use omni_core::archive::ArchiveSettings;
use omni_core::config::ArchiveConfig;
use omni_core::paths::AppPaths;

use crate::auth::{ApiError, RequireAdmin};
use crate::state::AppState;

/// How long a watchfolder change waits for running jobs, as the nightly yt-dlp swap.
pub const DRAIN: Duration = Duration::from_secs(600);
/// Between two tries of a pending change.
pub const RETRY_GAP: Duration = Duration::from_secs(300);
/// How long the save waits for the change before it answers "pending".
pub const ANSWER_WITHIN: Duration = Duration::from_secs(20);

/// A watchfolder saved but not in use yet: jobs were running.
#[derive(Debug, Clone, Serialize)]
pub struct PendingWatchfolder {
    pub path: String,
    pub since: DateTime<Utc>,
    pub attempts: u32,
}

/// The daemon's delivery paths as the panel changes them. Cheap to clone.
#[derive(Clone)]
pub struct LiveDelivery {
    pub paths: DeliveryPaths,
    /// The install, to resolve the archive's folder as the daemon does.
    pub install: AppPaths,
    drain: Duration,
    retry_gap: Duration,
    answer_within: Duration,
    pending: Arc<Mutex<Option<PendingWatchfolder>>>,
    /// Bumped by every save: an older pending change stops trying.
    generation: Arc<AtomicU64>,
    /// One switch at a time.
    apply_lock: Arc<tokio::sync::Mutex<()>>,
}

impl LiveDelivery {
    pub fn new(paths: DeliveryPaths, install: AppPaths) -> Self {
        Self {
            paths,
            install,
            drain: DRAIN,
            retry_gap: RETRY_GAP,
            answer_within: ANSWER_WITHIN,
            pending: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Shorter waits, for tests.
    pub fn with_timing(mut self, drain: Duration, retry_gap: Duration, answer_within: Duration) -> Self {
        (self.drain, self.retry_gap, self.answer_within) = (drain, retry_gap, answer_within);
        self
    }

    /// The change waiting for the queue to be quiet, if any.
    pub fn pending(&self) -> Option<PendingWatchfolder> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The archive `cfg` describes, checked against the watchfolder `wf`.
    pub fn archive_for(&self, cfg: &ArchiveConfig, wf: &Path) -> ArchiveSettings {
        ArchiveSettings::from_config(cfg, &self.install, wf)
    }

    fn set_pending(&self, generation: u64, f: impl FnOnce(&mut Option<PendingWatchfolder>)) {
        if self.generation.load(Ordering::SeqCst) == generation {
            f(&mut self.pending.lock().unwrap_or_else(|p| p.into_inner()));
        }
    }
}

/// What a watchfolder save did with the running daemon.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// In use now; this many waiting videos went to review.
    Now { reviewed: usize },
    /// Jobs are running: it applies when they have finished.
    Pending,
}

/// Put `resolved` (shown as `shown`) into use (P13.2). Answers within
/// [`ANSWER_WITHIN`]; a change that has not applied by then goes on in the
/// background, tried every [`RETRY_GAP`] until it applies or a newer save
/// replaces it.
pub async fn apply_watchfolder(state: &AppState, live: &LiveDelivery, resolved: PathBuf, shown: String, who: String) -> Applied {
    let generation = live.generation.fetch_add(1, Ordering::SeqCst) + 1;
    *live.pending.lock().unwrap_or_else(|p| p.into_inner()) =
        Some(PendingWatchfolder { path: shown, since: Utc::now(), attempts: 0 });
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(run_switch(state.clone(), live.clone(), resolved, generation, who, tx));
    match tokio::time::timeout(live.answer_within, rx).await {
        Ok(Ok(reviewed)) => Applied::Now { reviewed },
        _ => Applied::Pending,
    }
}

async fn run_switch(
    state: AppState,
    live: LiveDelivery,
    resolved: PathBuf,
    generation: u64,
    who: String,
    tx: tokio::sync::oneshot::Sender<usize>,
) {
    let mut tx = Some(tx);
    loop {
        {
            let _one = live.apply_lock.lock().await;
            if live.generation.load(Ordering::SeqCst) != generation {
                return; // a newer save took over
            }
            live.set_pending(generation, |p| {
                if let Some(p) = p {
                    p.attempts += 1;
                }
            });
            match switch_watchfolder(&state.update_gate, &live.paths, &state.repo, resolved.clone(), live.drain).await {
                Ok(WatchfolderSwitch::Switched { reviewed }) => {
                    let archive = state.config.read().await.archive.clone();
                    live.paths.set_archive(Some(live.archive_for(&archive, &resolved)));
                    live.set_pending(generation, |p| *p = None);
                    let _ = state.repo.log_audit(
                        "WARN",
                        "ADMIN",
                        &format!(
                            "Watchfolder {} in use (saved by {who}); {} finished video(s) waiting for the old one sent to review",
                            resolved.display(),
                            reviewed.len()
                        ),
                    );
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(reviewed.len());
                    }
                    return;
                }
                Ok(WatchfolderSwitch::Busy) => {
                    tracing::info!("Jobs still running; the new watchfolder waits for a quiet queue");
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "The watchfolder change could not be applied; it is tried again"),
            }
        }
        tokio::time::sleep(live.retry_gap).await;
    }
}

/// Restarts the Windows service from the panel (P13.2). The daemon implements
/// it; `None` in a console run and in tests, where the button says it cannot.
pub trait ServiceRestarter: Send + Sync {
    /// Start the helper that will start the service again once it has stopped.
    fn begin(&self) -> Result<(), String>;
    /// Ask the SCM to stop the service (the graceful stop of P2.15).
    fn stop(&self) -> Result<(), String>;
}

/// How long the restart waits before stopping, so the answer reaches the browser.
const STOP_AFTER: Duration = Duration::from_millis(800);

/// «Επανεκκίνηση υπηρεσίας» (P13.2): admin only, audited. The page asks
/// before calling it. Answers 202 and stops shortly after; the helper starts
/// the service again.
pub async fn api_admin_restart_service(RequireAdmin(admin): RequireAdmin, State(state): State<AppState>) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let Some(restarter) = state.restarter.clone() else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "NOT_A_SERVICE",
            "Η επανεκκίνηση από εδώ γίνεται μόνο όταν το πρόγραμμα τρέχει ως υπηρεσία των Windows. Κλείστε και ανοίξτε ξανά το παράθυρο εντολών.",
        ));
    };
    let begun = {
        let r = restarter.clone();
        tokio::task::spawn_blocking(move || r.begin()).await.map_err(|_| ApiError::internal("The restart could not start."))?
    };
    if let Err(why) = begun {
        tracing::error!(error = %why, "The restart helper did not start");
        let _ = state.repo.log_audit("ERROR", "ADMIN", &format!("Service restart asked by {} did not start: {why}", admin.email));
        return Err(ApiError::internal("Η επανεκκίνηση δεν ξεκίνησε· δείτε το αρχείο καταγραφής."));
    }
    let _ = state.repo.log_audit("WARN", "ADMIN", &format!("Service restart requested by {}", admin.email));
    let repo = state.repo.clone();
    tokio::spawn(async move {
        tokio::time::sleep(STOP_AFTER).await;
        if let Ok(Err(why)) = tokio::task::spawn_blocking(move || restarter.stop()).await {
            tracing::error!(error = %why, "The service could not be stopped for the restart");
            let _ = repo.log_audit(
                "ERROR",
                "ADMIN",
                &format!("The service could not stop itself for the restart ({why}). Run Setup again (Modify) so the service account may restart it, or restart it in Services"),
            );
        }
    });
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "status": "restarting" }))))
}
