use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use omni_core::models::{Job, JobStage};
use omni_core::repository::{HeldOutput, Repository};

use crate::delivery::WatchfolderDelivery;
use crate::downloader::{DownloadError, DownloadOpts, Downloader};
use crate::errors::ErrorCode;
use crate::preview::{self, PreviewEncoder};
use crate::rewrapper::Rewrapper;
use crate::transcoder::Transcoder;

/// `Err(why)` when the temp or watchfolder volume lacks room for a clip of
/// `duration_secs` (see [`omni_core::selftest::space_needed`]). A volume
/// whose free space cannot be read is not held against the job.
pub fn room_for(duration_secs: f64, temp: &std::path::Path, watchfolder: &std::path::Path) -> std::result::Result<(), String> {
    let (need_temp, need_watch) = omni_core::selftest::space_needed(duration_secs);
    let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    for (label, path, need) in [("temp", temp, need_temp), ("watchfolder", watchfolder, need_watch)] {
        if let Some((free, _)) = omni_core::selftest::disk_space(path) {
            if free < need {
                return Err(format!(
                    "Not enough disk space for a {:.0} min clip: {label} has {:.1} GB free, needs {:.1} GB",
                    duration_secs / 60.0,
                    gb(free),
                    gb(need)
                ));
            }
        }
    }
    Ok(())
}

/// Encoders running at once unless the daemon says otherwise. One MPEG-2
/// 50 Mbps 1080i encode keeps about six cores busy; two fill the reference
/// MCR machine (12 threads).
pub const DEFAULT_ENCODER_SLOTS: usize = 2;

/// The worker no longer holds the job's lease (discarded, requeued, or taken
/// by another worker). Carried inside the pipeline's error so the worker
/// can recognise it by type, not by text.
#[derive(Debug, thiserror::Error)]
#[error("job #{job_id} is no longer ours at {}", .stage.as_str())]
pub struct LeaseLost {
    pub job_id: i64,
    pub stage: JobStage,
}

/// What a finished, verified video does next (plan P10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Into the watchfolder, as always.
    Deliver,
    /// Keep the file under `temp/held/{id}` for MCR to approve; the
    /// watchfolder is never touched. `held_max_bytes` caps everything held
    /// at once.
    Hold { hold_hours: i64, held_max_bytes: u64 },
}

/// How a run that returned `Ok` ended (P10.5). The worker parks or
/// completes the job from this, not from what the database says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The file is in the watchfolder.
    Delivered,
    /// The file is held for MCR; nothing was delivered.
    Held,
}

fn is_cancelled(cancel: &Option<CancellationToken>) -> bool {
    cancel.as_ref().is_some_and(|t| t.is_cancelled())
}

#[derive(Clone)]
pub struct BroadcastEngine {
    repo: Repository,
    ytdl_path: PathBuf,
    ffmpeg_path: PathBuf,
    ffprobe_path: PathBuf,
    bmxtranswrap_path: PathBuf,
    temp_dir: PathBuf,
    watchfolder_dir: PathBuf,
    /// Encodes in progress (plan P1.13). Downloading is network-bound and
    /// encoding CPU-bound, so they are counted apart: a job finished
    /// downloading hands back its download slot and waits here, and the
    /// next download starts instead of sitting behind someone's encode.
    encoder_slots: Arc<Semaphore>,
    /// The download slot each job holds until it reaches the encoder.
    download_slots: Arc<Mutex<HashMap<i64, OwnedSemaphorePermit>>>,
    /// The H.264 encoder previews use, found on first use (P10.6).
    preview_encoder: Arc<tokio::sync::OnceCell<PreviewEncoder>>,
}

impl BroadcastEngine {
    /// At most `n` encodes at once (at least one).
    pub fn with_encoder_slots(mut self, n: usize) -> Self {
        self.encoder_slots = Arc::new(Semaphore::new(n.max(1)));
        self
    }

    /// Give the engine the download slot job `job_id` was leased under; it
    /// is released when the job starts waiting for an encoder, or by
    /// [`Self::release_download_slot`] when the job ends before that.
    pub fn hold_download_slot(&self, job_id: i64, permit: OwnedSemaphorePermit) {
        self.download_slots.lock().unwrap_or_else(|p| p.into_inner()).insert(job_id, permit);
    }

    /// Hand back job `job_id`'s download slot, if it still holds one.
    pub fn release_download_slot(&self, job_id: i64) {
        self.download_slots.lock().unwrap_or_else(|p| p.into_inner()).remove(&job_id);
    }

    /// Whether job `job_id` still holds its download slot.
    pub fn holds_download_slot(&self, job_id: i64) -> bool {
        self.download_slots.lock().unwrap_or_else(|p| p.into_inner()).contains_key(&job_id)
    }

    /// The encoder held-video previews use; asked of ffmpeg once, then kept.
    pub async fn preview_encoder(&self) -> PreviewEncoder {
        // A failed question is not remembered: the next hold asks again.
        let found = self
            .preview_encoder
            .get_or_try_init(|| async { preview::discover(&self.ffmpeg_path).await.ok_or(()) })
            .await;
        found.copied().unwrap_or(PreviewEncoder::None)
    }

    pub fn new(
        repo: Repository,
        ytdl_path: PathBuf,
        ffmpeg_path: PathBuf,
        ffprobe_path: PathBuf,
        bmxtranswrap_path: PathBuf,
        temp_dir: PathBuf,
        watchfolder_dir: PathBuf,
    ) -> Self {
        Self {
            repo,
            ytdl_path,
            ffmpeg_path,
            ffprobe_path,
            bmxtranswrap_path,
            temp_dir,
            watchfolder_dir,
            encoder_slots: Arc::new(Semaphore::new(DEFAULT_ENCODER_SLOTS)),
            download_slots: Arc::new(Mutex::new(HashMap::new())),
            preview_encoder: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// The videos yt-dlp sees on a news page that holds several, in page
    /// order, and whether yt-dlp has no extractor for it. See
    /// [`crate::downloader::PageScan`].
    pub async fn page_videos(&self, url: &str) -> crate::downloader::PageScan {
        crate::downloader::Downloader::new(&self.ytdl_path)
            .page_videos(url, std::time::Duration::from_secs(90))
            .await
    }

    /// Whether the post at `url` has a video, as yt-dlp's own extractor
    /// sees it without downloading (P3.13). `Some(false)` only when it says
    /// so ("No video could be found in this tweet"); a failure that proves
    /// nothing (network, login, timeout) is `None`.
    pub async fn post_has_video(&self, url: &str) -> Option<bool> {
        match Downloader::new(&self.ytdl_path).probe(url, std::time::Duration::from_secs(60)).await {
            Ok(_) => Some(true),
            Err(line) if crate::errors::classify_download_error(&line) == ErrorCode::NoStreamFound => Some(false),
            Err(_) => None,
        }
    }

    pub async fn process_job(&self, job: Job, owner: &str) -> Result<()> {
        self.process_job_with_context(job, owner, None, None, None).await
    }

    /// Move the job to `stage`, or stop for good if this worker no longer
    /// holds it (P1.14). A job MCR discarded, or one the reaper requeued and
    /// another worker took, must not reach the watchfolder from here. Writes
    /// no event and no audit row: the job is not ours to write to.
    async fn enter_stage(&self, job_id: i64, owner: &str, stage: JobStage, job_temp: &Path) -> Result<()> {
        match self.repo.set_stage(job_id, owner, stage) {
            Ok(true) => Ok(()),
            Ok(false) => Err(self.lost(job_id, stage, job_temp).await),
            // At Deliver an unknown lease state must not let a file go to air
            // for a job MCR may just have cancelled: fail, and let the worker's
            // normal retry path run.
            Err(e) if stage == JobStage::Deliver => Err(e.context(ErrorCode::DeliveryFailed.as_str())),
            Err(e) => {
                warn!("Job #{job_id}: could not record stage {}: {e:#}", stage.as_str());
                Ok(())
            }
        }
    }

    /// The error for a job that is no longer ours. Its workspace is removed
    /// when the row is gone (discarded) or CANCELLED: otherwise another
    /// worker may hold the very same `temp/jobs/{id}` and be reading its
    /// source.
    ///
    /// A cancelled job's directory goes too (P1.15): a killed download
    /// leaves a `.part`, and a retry or override of the same job would
    /// resume it into a different link. That is safe because the retry,
    /// override and redownload routes answer STILL_STOPPING while this
    /// worker's BusyJobs guard is alive, and this runs inside its task.
    async fn lost(&self, job_id: i64, stage: JobStage, job_temp: &Path) -> anyhow::Error {
        warn!("Job #{job_id}: no longer ours at {}; stopping without delivering", stage.as_str());
        match self.repo.get_job(job_id) {
            Ok(None) => WatchfolderDelivery::cleanup_job_temp_files(job_temp).await,
            Ok(Some(row)) if row.status == omni_core::models::JobStatus::Cancelled => {
                WatchfolderDelivery::cleanup_job_temp_files(job_temp).await
            }
            Ok(Some(_)) => {}
            Err(e) => warn!("Job #{job_id}: could not tell whether the job still exists, leaving its workspace: {e:#}"),
        }
        anyhow::Error::new(LeaseLost { job_id, stage }).context(ErrorCode::LeaseExpired.as_str())
    }

    /// Run the pipeline for a job the caller holds the lease on.
    ///
    /// `owner` is the lease owner string. Stage transitions go through
    /// `set_stage`, which refuses to advance a job this worker no longer owns --
    /// so a worker whose lease was reaped mid-transcode cannot keep driving a
    /// job another worker has already picked up. The engine never sets a
    /// terminal status; the worker that holds the lease decides that, so the two
    /// cannot race each other into an inconsistent row.
    pub async fn process_job_with_context(
        &self,
        job: Job,
        owner: &str,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
    ) -> Result<()> {
        self.process_job_cancellable(job, owner, referer, user_agent, cookies, None).await
    }

    /// [`Self::process_job_with_context`] with the run's token (plan P1.15).
    /// When it is cancelled the running tool is killed, and whatever error
    /// the stage then produces comes out as [`LeaseLost`]: the worker writes
    /// nothing for a job somebody else (MCR) has taken.
    pub async fn process_job_cancellable(
        &self,
        job: Job,
        owner: &str,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
        cancel: Option<CancellationToken>,
    ) -> Result<()> {
        self.process_job_mode(job, owner, referer, user_agent, cookies, cancel, DeliveryMode::Deliver)
            .await
            .map(|_| ())
    }

    /// [`Self::process_job_cancellable`] with what to do with the finished
    /// file (P10.5). `Hold` stops after the compliance gate, keeps the file
    /// and returns `Ok`; the worker then parks the job as AWAITING_APPROVAL
    /// (it, not the engine, sets the status).
    pub async fn process_job_mode(
        &self,
        job: Job,
        owner: &str,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
        cancel: Option<CancellationToken>,
        delivery: DeliveryMode,
    ) -> Result<RunOutcome> {
        let job_id = job.id;
        let slug = job.slug.clone();
        let url = job.url.clone();

        // Every stage works inside this job's own directory. Cleanup is a
        // remove_dir_all of exactly this path -- never a filename-prefix match
        // in a shared directory, which is how job 1 used to delete the working
        // files of jobs 10-19 and 100-199 mid-transcode (defect D-04).
        let job_temp = self.temp_dir.join("jobs").join(job_id.to_string());
        tokio::fs::create_dir_all(&job_temp).await.with_context(|| {
            format!("Failed creating the workspace for job #{job_id} at {job_temp:?}")
        })?;

        info!("BroadcastEngine: Processing Job #{} ({})", job_id, slug);

        // 1a. A video attached to an email (plan P4.6) is already on disk:
        // the mail watcher saved it and recorded `source_path`. It lives
        // outside the job workspace, so a failed attempt's cleanup below does
        // not take the only copy with it and a retry still has its source.
        if url.starts_with("attachment://") {
            let source = job.source_path.as_deref().map(PathBuf::from).filter(|p| p.is_file());
            let Some(source) = source else {
                let err_msg = "The attachment is not on disk; download it from the email by hand";
                warn!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Download), err_msg);
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(anyhow::anyhow!(err_msg).context(ErrorCode::ManualDownload.as_str()));
            };
            let _ = self.repo.record_event(
                job_id,
                "INFO",
                Some(JobStage::Download),
                "Source is the file attached to the email; nothing to download",
            );
            return self.finish_from_source(job, owner, source, job_temp, cancel, delivery).await;
        }

        // 1. Download stage
        let downloader = Downloader::new(&self.ytdl_path);
        let repo_clone = self.repo.clone();
        let raw_download_res = downloader
            .download(
                job_id,
                &url,
                &job_temp,
                DownloadOpts {
                    referer,
                    user_agent,
                    cookie_header: cookies,
                    cancel: cancel.clone(),
                    ..Default::default()
                },
                move |prog| {
                    let _ = repo_clone.update_job_progress(
                        job_id,
                        prog.percent,
                        &prog.speed,
                        &prog.eta,
                    );
                },
            )
            .await;

        let downloaded_file = match raw_download_res {
            Ok(path) => path,
            Err(e) => {
                if is_cancelled(&cancel) {
                    return Err(self.lost(job_id, JobStage::Download, &job_temp).await);
                }
                // The downloader already classified the failure, so the pipeline
                // does not have to re-read stderr to decide whether a retry could
                // possibly help (plan P1.9).
                let code = e
                    .downcast_ref::<DownloadError>()
                    .map(|d| d.code)
                    .unwrap_or(ErrorCode::PipelineFailed);
                let err_msg = format!("{code}: {e}");
                warn!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Download), &err_msg);
                let _ = self.repo.log_audit(
                    "WARN",
                    "DOWNLOAD",
                    &format!("Job #{job_id} ({slug}): {err_msg}"),
                );
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e.context(code.as_str()));
            }
        };

        self.finish_from_source(job, owner, downloaded_file, job_temp, cancel, delivery).await
    }

    /// Stages 2–6, from a source file on disk to the watchfolder.
    async fn finish_from_source(
        &self,
        job: Job,
        owner: &str,
        downloaded_file: PathBuf,
        job_temp: PathBuf,
        cancel: Option<CancellationToken>,
        delivery: DeliveryMode,
    ) -> Result<RunOutcome> {
        let job_id = job.id;
        let slug = job.slug.clone();

        // 2. Transcode stage (Sony XDCAM HD422 PAL 1080i50)
        if let Err(e) = self.enter_stage(job_id, owner, JobStage::Transcode, &job_temp).await {
            // Not ours any more: the next download must not wait on this slot.
            self.release_download_slot(job_id);
            return Err(e);
        }

        // The source is on disk: let the next download start, then wait for
        // an encoder (plan P1.13). Held until the encode ends; rewrap and
        // delivery are disk-bound and do not need it.
        self.release_download_slot(job_id);
        let encoder = match self.encoder_slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                let _ = self.repo.update_job_progress(job_id, 0.0, "Waiting for encoder", "--:--");
                let _ = self.repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Transcode),
                    "Downloaded; waiting for a free encoder",
                );
                let wait = self.encoder_slots.clone().acquire_owned();
                match &cancel {
                    Some(t) => tokio::select! {
                        _ = t.cancelled() => return Err(self.lost(job_id, JobStage::Transcode, &job_temp).await),
                        p = wait => p.context("encoder slots closed")?,
                    },
                    None => wait.await.context("encoder slots closed")?,
                }
            }
        };
        // Discarded or taken while it waited for an encoder: do not burn
        // minutes of encode on it.
        if !self.repo.owns_lease(job_id, owner).unwrap_or(true) {
            drop(encoder);
            return Err(self.lost(job_id, JobStage::Transcode, &job_temp).await);
        }
        let _ = self.repo.update_job_progress(job_id, 0.0, "Transcoding", "--:--");

        let mut transcoder = Transcoder::new(&self.ffmpeg_path, &self.ffprobe_path);
        if let Some(t) = &cancel {
            transcoder = transcoder.with_cancel(t.clone());
        }

        // Room for this file before it is made (plan P1.4): a full disk
        // mid-encode fails late, after minutes of work, and can take the
        // database down with it. Waiting is the fix (LOW_DISK retries in
        // 10 min), so the job goes back to the queue, not to review.
        if let Ok(probe) = transcoder.probe_source(&downloaded_file).await {
            if let Err(why) = room_for(probe.duration_secs, &job_temp, &self.watchfolder_dir) {
                warn!("Job #{job_id}: {why}");
                let _ = self.repo.record_event(job_id, "WARN", Some(JobStage::Transcode), &why);
                drop(encoder);
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(anyhow::anyhow!("{}: {why}", ErrorCode::LowDisk.as_str()).context(ErrorCode::LowDisk.as_str()));
            }
        }
        let repo_clone = self.repo.clone();
        let transcode_res = transcoder
            .transcode(job_id, &downloaded_file, &job_temp, move |prog| {
                let _ = repo_clone.update_job_progress(
                    job_id,
                    prog.percent,
                    &prog.speed,
                    &prog.eta,
                );
            })
            .await;
        drop(encoder);

        let (intermediate_mxf, duration) = match transcode_res {
            Ok(res) => res,
            Err(e) => {
                if is_cancelled(&cancel) {
                    return Err(self.lost(job_id, JobStage::Transcode, &job_temp).await);
                }
                let err_msg = format!("FFmpeg Transcode failed: {e:#}");
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "TRANSCODE", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };

        // 3. Rewrap stage (SMPTE RDD9 OP1a MXF)
        self.enter_stage(job_id, owner, JobStage::Rewrap, &job_temp).await?;
        // A rename by MCR up to now names the file (P7.12). Read after the
        // stage is REWRAP, which is when a rename starts being refused, so
        // one either lands here or is turned down; never half-applied.
        let slug = self.repo.get_job(job_id).ok().flatten().map(|j| j.slug).unwrap_or(slug);
        let _ = self.repo.update_job_progress(job_id, 99.0, "Rewrapping", "--:--");

        let rewrapper = Rewrapper::new(&self.bmxtranswrap_path);
        let final_temp_mxf = match rewrapper
            .rewrap_with_clip(job_id, &intermediate_mxf, &job_temp, Some(&slug), cancel.clone())
            .await {
            Ok(path) => path,
            Err(e) => {
                if is_cancelled(&cancel) {
                    return Err(self.lost(job_id, JobStage::Rewrap, &job_temp).await);
                }
                let err_msg = format!("bmxtranswrap RDD9 failed: {e:#}");
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "REWRAP", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };
        // The intermediate has served its purpose: dropping it now halves
        // what a long clip holds on the temp disk until delivery (P1.4).
        let _ = tokio::fs::remove_file(&intermediate_mxf).await;

        // 4. Compliance gate (plan P1.6, defect D-06).
        //
        // The last thing between the transcoder and air. ffmpeg exits zero for
        // plenty of files Dalet will reject or mis-play -- a chain that quietly
        // fell back to 4:2:0, an audio map that produced one stereo stream
        // instead of eight mono ones, a transcode truncated by a source that
        // ended early. A failed report is never delivered: a job in review is a
        // minor annoyance, a wrong file in the running order is not.
        self.enter_stage(job_id, owner, JobStage::Verify, &job_temp).await?;
        let report = match crate::verify::verify_mxf_with_clip(&self.ffprobe_path, &final_temp_mxf, duration, Some(&slug))
            .await {
            Ok(report) => report,
            Err(e) => {
                // Could not run the check at all. Treated as a failure: an
                // unverifiable file must not reach the watchfolder.
                let err_msg = format!("Compliance check could not run: {e}");
                error!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Verify), &err_msg);
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e.context("COMPLIANCE_FAILED"));
            }
        };

        if let Ok(json) = serde_json::to_string(&report) {
            let _ = self.repo.set_compliance_report(job_id, &json);
        }

        if !report.pass {
            let summary = report.summary();
            error!("Job #{job_id}: {summary}");
            let _ = self
                .repo
                .record_event(job_id, "ERROR", Some(JobStage::Verify), &summary);
            let _ = self.repo.log_audit(
                "ERROR",
                "COMPLIANCE",
                &format!("Job #{job_id} ({slug}) failed the compliance gate: {summary}"),
            );
            WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
            return Err(anyhow::anyhow!("COMPLIANCE_FAILED: {summary}"));
        }

        let _ = self.repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Verify),
            &report.summary(),
        );

        // 4b. Held for MCR: the file stays here, bound to its SHA-256, and
        // nothing reaches the watchfolder until it is approved (P10.5).
        if let DeliveryMode::Hold { hold_hours, held_max_bytes } = delivery {
            self.hold_final(job_id, owner, &final_temp_mxf, &slug, duration, hold_hours, held_max_bytes, &job_temp, cancel.as_ref())
                .await?;
            return Ok(RunOutcome::Held);
        }

        // 5. Atomic Delivery to Dalet Watchfolder
        self.enter_stage(job_id, owner, JobStage::Deliver, &job_temp).await?;
        let delivered = match WatchfolderDelivery::deliver(&final_temp_mxf, &self.watchfolder_dir, &slug).await {
            Ok(dest) => dest,
            Err(e) => {
                let err_msg = format!("Watchfolder delivery failed: {e:#}");
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };

        // Clean up remaining temp files for this job
        WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;

        if delivered.suffixed {
            // Two assets now share a slug in Dalet. Not a failure -- the file is
            // on air -- but an operator has to know which one is which.
            let _ = self.repo.record_event(
                job_id,
                "WARN",
                Some(JobStage::Deliver),
                &format!(
                    "Slug collision: delivered as {} because {slug}.mxf already existed",
                    delivered.filename
                ),
            );
        }

        // 6. Record the file. The status is not set here: the worker that
        // holds the lease finishes the job as Completed with this file_path.
        let dest_str = delivered.path.to_string_lossy().to_string();
        let _ = self.repo.update_job_progress(job_id, 100.0, "Completed", "00:00");
        match self.repo.set_delivered_file(job_id, owner, &dest_str, duration) {
            Ok(true) => {}
            Ok(false) => warn!("Job #{job_id}: lease lost after delivery; the file {dest_str} is in the watchfolder"),
            Err(e) => warn!("Job #{job_id}: could not record the delivered file {dest_str}: {e:#}"),
        }
        let _ = self.repo.log_audit(
            "SUCCESS",
            "INGEST",
            &format!("Job #{} ({}) ready in watchfolder at {:?}", job_id, slug, dest_str),
        );

        info!("Job #{} ({}) broadcast ingest successfully finished!", job_id, slug);
        Ok(RunOutcome::Delivered)
    }

    /// The encoder slots (for tests and diagnostics): a deliver-only job
    /// must never wait on them.
    pub fn encoder_slots(&self) -> Arc<Semaphore> {
        self.encoder_slots.clone()
    }

    /// Remove job `job_id`'s held directory (best effort; the sweep retries).
    pub fn discard_held_dir(&self, job_id: i64) {
        omni_core::held::remove_held_dir(&self.temp_dir, job_id);
    }

    /// `temp/held/{id}` for this engine's temp directory.
    fn held_dir(&self, job_id: i64) -> PathBuf {
        omni_core::held::held_dir(&self.temp_dir, job_id)
    }

    /// The `Hold` end of the pipeline: the verified file leaves the job's
    /// workspace for `temp/held/{id}/{slug}.mxf`, its SHA-256 is recorded,
    /// and the job's workspace is removed. Never touches the watchfolder.
    async fn hold_final(
        &self,
        job_id: i64,
        owner: &str,
        final_mxf: &Path,
        slug: &str,
        duration: f64,
        hold_hours: i64,
        held_max_bytes: u64,
        job_temp: &Path,
        cancel: Option<&CancellationToken>,
    ) -> Result<()> {
        if !self.repo.owns_lease(job_id, owner).unwrap_or(false) {
            return Err(self.lost(job_id, JobStage::Verify, job_temp).await);
        }
        let bytes = tokio::fs::metadata(final_mxf)
            .await
            .with_context(|| format!("Could not read the size of the file to hold: {final_mxf:?}"))
            .context(ErrorCode::PipelineFailed.as_str())?
            .len();
        let in_use = self
            .repo
            .held_bytes_in_use(job_id)
            .context("Could not read what is already held for approval")
            .context(ErrorCode::PipelineFailed.as_str())?;
        if in_use.saturating_add(bytes) > held_max_bytes {
            let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
            let why = format!(
                "Not enough room to hold the video for approval: {:.1} GB already held, this one is {:.1} GB, the limit is {:.1} GB",
                gb(in_use),
                gb(bytes),
                gb(held_max_bytes)
            );
            warn!("Job #{job_id}: {why}");
            let _ = self.repo.record_event(job_id, "WARN", Some(JobStage::Verify), &why);
            WatchfolderDelivery::cleanup_job_temp_files(job_temp).await;
            return Err(anyhow::anyhow!("{}: {why}", ErrorCode::LowDisk.as_str()).context(ErrorCode::LowDisk.as_str()));
        }

        // A fresh directory: an older hold of this job (retried since) goes.
        let dir = self.held_dir(job_id);
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir)
                .await
                .with_context(|| format!("Could not clear the old held directory {dir:?}"))
                .context(ErrorCode::PipelineFailed.as_str())?;
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("Could not create the held directory {dir:?}"))
            .context(ErrorCode::PipelineFailed.as_str())?;
        let held_path = dir.join(format!("{slug}.mxf"));
        if tokio::fs::rename(final_mxf, &held_path).await.is_err() {
            // Another volume: copy, then remove the original.
            let copied = tokio::fs::copy(final_mxf, &held_path).await;
            if let Err(e) = copied {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(anyhow::Error::new(e)
                    .context(format!("Could not move the video to {held_path:?}"))
                    .context(ErrorCode::PipelineFailed.as_str()));
            }
            let _ = tokio::fs::remove_file(final_mxf).await;
        }

        let hash_path = held_path.clone();
        let hashed = tokio::task::spawn_blocking(move || omni_core::held::sha256_file(&hash_path)).await;
        let sha256 = match hashed {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(anyhow::Error::new(e)
                    .context("Could not fingerprint the held video")
                    .context(ErrorCode::PipelineFailed.as_str()));
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(anyhow::Error::new(e)
                    .context("Could not fingerprint the held video")
                    .context(ErrorCode::PipelineFailed.as_str()));
            }
        };

        let expires_at = chrono::Utc::now() + chrono::Duration::hours(hold_hours.max(1));
        let recorded = self.repo.hold_output(
            job_id,
            owner,
            &held_path.to_string_lossy(),
            bytes as i64,
            &sha256,
            Some(duration),
            expires_at,
        );
        match recorded {
            Ok(true) => {}
            Ok(false) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(self.lost(job_id, JobStage::Verify, job_temp).await);
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(e.context("Could not record the held video").context(ErrorCode::PipelineFailed.as_str()));
            }
        }

        // A look at exactly the held file for MCR; best effort, never fails the hold.
        let token = cancel.cloned().unwrap_or_default();
        let encoder = self.preview_encoder().await;
        let made = preview::make(&self.ffmpeg_path, &held_path, &dir, encoder, duration, &token).await;
        if !made.has_preview() {
            let why = made.why.as_deref().unwrap_or("άγνωστος λόγος");
            let _ = self.repo.record_event(
                job_id,
                "WARN",
                Some(JobStage::Verify),
                &format!("Δεν έγινε προεπισκόπηση: {why}"),
            );
        } else {
            let _ = self.repo.record_event(job_id, "INFO", Some(JobStage::Verify), "Προεπισκόπηση έτοιμη");
        }

        WatchfolderDelivery::cleanup_job_temp_files(job_temp).await;
        let _ = self.repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Verify),
            "Το βίντεο περιμένει έγκριση από το MCR",
        );
        info!("Job #{job_id} ({slug}) is held for approval ({bytes} bytes, sha256 {sha256})");
        Ok(())
    }

    /// Deliver the held file MCR approved, and nothing else (P10.5). The
    /// file passes the compliance gate again; then, at the last moment, it
    /// is opened so that nobody can change or delete it, hashed through that
    /// very handle against the SHA-256 recorded when it was held, and copied
    /// by the same [`WatchfolderDelivery::deliver`] as any other video while
    /// the handle is still open. Takes no download slot and no encoder slot:
    /// nothing is downloaded or encoded.
    ///
    /// The file's path in the watchfolder is recorded on the job and on the
    /// held row together right after the copy; a run that finds both equal
    /// (a crash before the worker finished the job) copies nothing again.
    /// The held row stays `approved` and the held directory stays: the
    /// worker holding the lease finishes the job and the row together
    /// (`Repository::finish_held_delivery`) and then calls
    /// [`Self::discard_held_dir`]. The engine sets no terminal status.
    pub async fn deliver_held(
        &self,
        job: &Job,
        owner: &str,
        held: &HeldOutput,
        cancel: Option<CancellationToken>,
    ) -> Result<()> {
        let job_id = job.id;
        let job_temp = self.temp_dir.join("jobs").join(job_id.to_string());
        // A deliver-only job has no use for the slot it was leased under.
        self.release_download_slot(job_id);

        // Delivered by an earlier run of this approval, then the worker died.
        if let (Some(done), Some(at)) = (held.delivered_path.as_deref(), job.file_path.as_deref()) {
            if !done.is_empty() && done == at {
                info!("Job #{job_id}: the approved video is already at {at}; nothing to copy");
                let _ = self.repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Deliver),
                    &format!("Already in the watchfolder at {at}; not copied again"),
                );
                return Ok(());
            }
        }

        self.enter_stage(job_id, owner, JobStage::Verify, &job_temp).await?;
        let path = PathBuf::from(&held.path);
        // The file is the one recorded for this job, in its own directory.
        let slug = match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) if path.starts_with(self.held_dir(job_id)) => stem.to_string(),
            _ => return Err(self.held_changed(job_id, &format!("the held path {path:?} is not in this job's held directory"))),
        };
        // Gone or a different size: no need to run the checks below.
        match tokio::fs::metadata(&path).await {
            Ok(m) if m.is_file() && m.len() as i64 == held.bytes => {}
            Ok(_) => return Err(self.held_changed(job_id, "its size is not the one that was approved")),
            Err(e) => return Err(self.held_changed(job_id, &format!("it cannot be read: {e}"))),
        }
        if is_cancelled(&cancel) {
            return Err(self.lost(job_id, JobStage::Verify, &job_temp).await);
        }

        let duration = held.duration.unwrap_or(0.0);
        let report = match crate::verify::verify_mxf_with_clip(&self.ffprobe_path, &path, duration, Some(&slug)).await {
            Ok(report) => report,
            Err(e) => {
                let err_msg = format!("Compliance check could not run: {e}");
                error!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Verify), &err_msg);
                return Err(e.context("COMPLIANCE_FAILED"));
            }
        };
        if let Ok(json) = serde_json::to_string(&report) {
            let _ = self.repo.set_compliance_report(job_id, &json);
        }
        if !report.pass {
            let summary = report.summary();
            error!("Job #{job_id}: {summary}");
            let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Verify), &summary);
            let _ = self.repo.log_audit(
                "ERROR",
                "COMPLIANCE",
                &format!("Job #{job_id} ({slug}) failed the compliance gate before delivery: {summary}"),
            );
            return Err(anyhow::anyhow!("COMPLIANCE_FAILED: {summary}"));
        }
        let _ = self.repo.record_event(job_id, "INFO", Some(JobStage::Verify), &report.summary());

        self.enter_stage(job_id, owner, JobStage::Deliver, &job_temp).await?;

        // Hash right before the copy, through a handle nobody can write to
        // or delete from, and keep it open until the copy is done.
        let lock_path = path.clone();
        let hashed = tokio::task::spawn_blocking(move || {
            let file = omni_core::held::open_locked(&lock_path)?;
            let sha = omni_core::held::sha256_reader(&file)?;
            // Hashing moved the position; the handle only guards the file.
            Ok::<_, std::io::Error>((sha, file))
        })
        .await;
        let (sha, guard) = match hashed {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(self.held_changed(job_id, &format!("it cannot be read: {e}"))),
            Err(e) => return Err(anyhow::Error::new(e).context(ErrorCode::PipelineFailed.as_str())),
        };
        if !sha.eq_ignore_ascii_case(&held.sha256) {
            return Err(self.held_changed(job_id, "its SHA-256 is not the one that was approved"));
        }

        let delivered = match WatchfolderDelivery::deliver(&path, &self.watchfolder_dir, &slug).await {
            Ok(dest) => dest,
            Err(e) => {
                drop(guard);
                let err_msg = format!("Watchfolder delivery failed: {e:#}");
                error!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{job_id} ({slug}): {err_msg}"));
                return Err(e);
            }
        };
        drop(guard);
        if delivered.suffixed {
            let _ = self.repo.record_event(
                job_id,
                "WARN",
                Some(JobStage::Deliver),
                &format!("Slug collision: delivered as {} because {slug}.mxf already existed", delivered.filename),
            );
        }

        let dest_str = delivered.path.to_string_lossy().to_string();
        let _ = self.repo.update_job_progress(job_id, 100.0, "Completed", "00:00");
        match self.repo.record_held_delivery(job_id, owner, &dest_str, duration) {
            Ok(true) => {}
            Ok(false) => {
                // On air but not ours to record. The row stays approved and
                // the held file stays; nothing is marked delivered.
                warn!("Job #{job_id}: lease lost after delivery; the file {dest_str} is in the watchfolder");
                return Err(self.lost(job_id, JobStage::Deliver, &job_temp).await);
            }
            Err(e) => {
                warn!("Job #{job_id}: could not record the delivered file {dest_str}: {e:#}");
                return Err(e.context("Could not record the delivery").context(ErrorCode::PipelineFailed.as_str()));
            }
        }
        let _ = self.repo.log_audit(
            "SUCCESS",
            "INGEST",
            &format!("Job #{job_id} ({slug}) approved by MCR and ready in watchfolder at {dest_str:?}"),
        );
        info!("Job #{job_id} ({slug}) delivered after approval");
        Ok(())
    }

    /// The held file is not what was approved: stop, deliver nothing.
    fn held_changed(&self, job_id: i64, why: &str) -> anyhow::Error {
        let err_msg = format!("The video that waited for approval changed or is gone, and was not delivered: {why}");
        error!("Job #{job_id}: {err_msg}");
        let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Verify), &err_msg);
        let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{job_id}: {err_msg}"));
        anyhow::anyhow!("{}: {err_msg}", ErrorCode::HeldFileChanged.as_str()).context(ErrorCode::HeldFileChanged.as_str())
    }
}
