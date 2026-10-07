use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{error, info, warn};

use omni_core::models::{Job, JobStage, JobStatus};
use omni_core::repository::Repository;

use crate::delivery::WatchfolderDelivery;
use crate::downloader::{DownloadError, DownloadOpts, Downloader};
use crate::errors::ErrorCode;
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
            return self.finish_from_source(job, owner, source, job_temp).await;
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

        self.finish_from_source(job, owner, downloaded_file, job_temp).await
    }

    /// Stages 2–6, from a source file on disk to the watchfolder.
    async fn finish_from_source(&self, job: Job, owner: &str, downloaded_file: PathBuf, job_temp: PathBuf) -> Result<()> {
        let job_id = job.id;
        let slug = job.slug.clone();

        // 2. Transcode stage (Sony XDCAM HD422 PAL 1080i50)
        let _ = self.repo.set_stage(job_id, owner, JobStage::Transcode);

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
                self.encoder_slots.clone().acquire_owned().await.context("encoder slots closed")?
            }
        };
        let _ = self.repo.update_job_progress(job_id, 0.0, "Transcoding", "--:--");

        let transcoder = Transcoder::new(&self.ffmpeg_path, &self.ffprobe_path);

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
                let err_msg = format!("FFmpeg Transcode failed: {e:#}");
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "TRANSCODE", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };

        // 3. Rewrap stage (SMPTE RDD9 OP1a MXF)
        let _ = self.repo.set_stage(job_id, owner, JobStage::Rewrap);
        // A rename by MCR up to now names the file (P7.12). Read after the
        // stage is REWRAP, which is when a rename starts being refused, so
        // one either lands here or is turned down; never half-applied.
        let slug = self.repo.get_job(job_id).ok().flatten().map(|j| j.slug).unwrap_or(slug);
        let _ = self.repo.update_job_progress(job_id, 99.0, "Rewrapping", "--:--");

        let rewrapper = Rewrapper::new(&self.bmxtranswrap_path);
        let final_temp_mxf = match rewrapper
            .rewrap_with_clip(job_id, &intermediate_mxf, &job_temp, Some(&slug), None)
            .await {
            Ok(path) => path,
            Err(e) => {
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
        let _ = self.repo.set_stage(job_id, owner, JobStage::Verify);
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

        // 5. Atomic Delivery to Dalet Watchfolder
        let _ = self.repo.set_stage(job_id, owner, JobStage::Deliver);
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

        // 6. Mark Completed
        let dest_str = delivered.path.to_string_lossy().to_string();
        let _ = self.repo.update_job_progress(job_id, 100.0, "Completed", "00:00");
        let _ = self.repo.update_job_status(job_id, JobStatus::Completed, None, Some(&dest_str), Some(duration));
        let _ = self.repo.log_audit(
            "SUCCESS",
            "INGEST",
            &format!("Job #{} ({}) ready in watchfolder at {:?}", job_id, slug, dest_str),
        );

        info!("Job #{} ({}) broadcast ingest successfully finished!", job_id, slug);
        Ok(())
    }
}
