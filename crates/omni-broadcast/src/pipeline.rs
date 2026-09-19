use anyhow::Result;
use std::path::PathBuf;
use tracing::{error, info, warn};

use omni_core::models::{Job, JobStatus};
use omni_core::repository::Repository;

use crate::delivery::WatchfolderDelivery;
use crate::downloader::Downloader;
use crate::rewrapper::Rewrapper;
use crate::transcoder::Transcoder;

#[derive(Clone)]
pub struct BroadcastEngine {
    repo: Repository,
    ytdl_path: PathBuf,
    ffmpeg_path: PathBuf,
    ffprobe_path: PathBuf,
    bmxtranswrap_path: PathBuf,
    temp_dir: PathBuf,
    watchfolder_dir: PathBuf,
}

impl BroadcastEngine {
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
        }
    }

    pub async fn process_job(&self, job: Job) -> Result<()> {
        self.process_job_with_context(job, None, None, None).await
    }

    pub async fn process_job_with_context(
        &self,
        job: Job,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
    ) -> Result<()> {
        let job_id = job.id;
        let slug = job.slug.clone();
        let url = job.url.clone();

        info!("BroadcastEngine: Processing Job #{} ({})", job_id, slug);

        // 1. Download stage
        let downloader = Downloader::new(&self.ytdl_path);
        let repo_clone = self.repo.clone();
        let raw_download_res = downloader
            .download_with_context(
                job_id,
                &url,
                &slug,
                &self.temp_dir,
                referer,
                user_agent,
                cookies,
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
                let err_msg = format!("Download failed: {}", e);
                warn!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.update_job_status(job_id, JobStatus::RequiresReview, Some(&err_msg), None, None);
                let _ = self.repo.log_audit("WARN", "DOWNLOAD", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&self.temp_dir, job_id).await;
                return Err(e);
            }
        };

        // 2. Transcode stage (Sony XDCAM HD422 PAL 1080i50)
        let _ = self.repo.update_job_status(job_id, JobStatus::Transcoding, None, None, None);
        let _ = self.repo.update_job_progress(job_id, 0.0, "Transcoding", "--:--");

        let transcoder = Transcoder::new(&self.ffmpeg_path, &self.ffprobe_path);
        let repo_clone = self.repo.clone();
        let transcode_res = transcoder
            .transcode(job_id, &downloaded_file, &self.temp_dir, move |prog| {
                let _ = repo_clone.update_job_progress(
                    job_id,
                    prog.percent,
                    &prog.speed,
                    &prog.eta,
                );
            })
            .await;

        let (intermediate_mxf, duration) = match transcode_res {
            Ok(res) => res,
            Err(e) => {
                let err_msg = format!("FFmpeg Transcode failed: {}", e);
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.update_job_status(job_id, JobStatus::RequiresReview, Some(&err_msg), None, None);
                let _ = self.repo.log_audit("ERROR", "TRANSCODE", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&self.temp_dir, job_id).await;
                return Err(e);
            }
        };

        // 3. Rewrap stage (SMPTE RDD9 OP1a MXF)
        let _ = self.repo.update_job_status(job_id, JobStatus::Rewrapping, None, None, None);
        let _ = self.repo.update_job_progress(job_id, 99.0, "Rewrapping", "--:--");

        let rewrapper = Rewrapper::new(&self.bmxtranswrap_path);
        let final_temp_mxf = match rewrapper.rewrap(job_id, &intermediate_mxf, &self.temp_dir).await {
            Ok(path) => path,
            Err(e) => {
                let err_msg = format!("bmxtranswrap RDD9 failed: {}", e);
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.update_job_status(job_id, JobStatus::RequiresReview, Some(&err_msg), None, None);
                let _ = self.repo.log_audit("ERROR", "REWRAP", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&self.temp_dir, job_id).await;
                return Err(e);
            }
        };

        // 4. Atomic Delivery to Dalet Watchfolder
        let final_destination = match WatchfolderDelivery::deliver(&final_temp_mxf, &self.watchfolder_dir, &slug).await {
            Ok(dest) => dest,
            Err(e) => {
                let err_msg = format!("Watchfolder delivery failed: {}", e);
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.update_job_status(job_id, JobStatus::RequiresReview, Some(&err_msg), None, None);
                let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&self.temp_dir, job_id).await;
                return Err(e);
            }
        };

        // Clean up remaining temp files for this job
        WatchfolderDelivery::cleanup_job_temp_files(&self.temp_dir, job_id).await;

        // 5. Mark Completed
        let dest_str = final_destination.to_string_lossy().to_string();
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
