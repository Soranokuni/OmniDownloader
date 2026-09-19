use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::{error, info, warn};

use omni_core::models::{Job, JobStage, JobStatus};
use omni_core::repository::Repository;

use crate::delivery::WatchfolderDelivery;
use crate::downloader::{DownloadError, DownloadOpts, Downloader};
use crate::errors::ErrorCode;
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

        // 2. Transcode stage (Sony XDCAM HD422 PAL 1080i50)
        let _ = self.repo.set_stage(job_id, owner, JobStage::Transcode);
        let _ = self.repo.update_job_progress(job_id, 0.0, "Transcoding", "--:--");

        let transcoder = Transcoder::new(&self.ffmpeg_path, &self.ffprobe_path);
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

        let (intermediate_mxf, duration) = match transcode_res {
            Ok(res) => res,
            Err(e) => {
                let err_msg = format!("FFmpeg Transcode failed: {}", e);
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "TRANSCODE", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };

        // 3. Rewrap stage (SMPTE RDD9 OP1a MXF)
        let _ = self.repo.set_stage(job_id, owner, JobStage::Rewrap);
        let _ = self.repo.update_job_progress(job_id, 99.0, "Rewrapping", "--:--");

        let rewrapper = Rewrapper::new(&self.bmxtranswrap_path);
        let final_temp_mxf = match rewrapper.rewrap(job_id, &intermediate_mxf, &job_temp).await {
            Ok(path) => path,
            Err(e) => {
                let err_msg = format!("bmxtranswrap RDD9 failed: {}", e);
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "REWRAP", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };

        // 4. Compliance gate (plan P1.6, defect D-06).
        //
        // The last thing between the transcoder and air. ffmpeg exits zero for
        // plenty of files Dalet will reject or mis-play -- a chain that quietly
        // fell back to 4:2:0, an audio map that produced one stereo stream
        // instead of eight mono ones, a transcode truncated by a source that
        // ended early. A failed report is never delivered: a job in review is a
        // minor annoyance, a wrong file in the running order is not.
        let _ = self.repo.set_stage(job_id, owner, JobStage::Verify);
        let report = match crate::verify::verify_mxf(&self.ffprobe_path, &final_temp_mxf, duration).await {
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
                let err_msg = format!("Watchfolder delivery failed: {}", e);
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
