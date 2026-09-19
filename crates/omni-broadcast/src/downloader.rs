use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

pub struct DownloadProgress {
    pub percent: f64,
    pub speed: String,
    pub eta: String,
}

pub struct Downloader {
    ytdl_path: PathBuf,
}

impl Downloader {
    pub fn new<P: AsRef<Path>>(ytdl_path: P) -> Self {
        Self {
            ytdl_path: ytdl_path.as_ref().to_path_buf(),
        }
    }

    pub async fn download<F>(
        &self,
        job_id: i64,
        url: &str,
        slug: &str,
        temp_dir: &Path,
        on_progress: F,
    ) -> Result<PathBuf>
    where
        F: FnMut(DownloadProgress) + Send + 'static,
    {
        self.download_with_context(job_id, url, slug, temp_dir, None, None, None, on_progress)
            .await
    }

    pub async fn download_with_context<F>(
        &self,
        job_id: i64,
        url: &str,
        slug: &str,
        temp_dir: &Path,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
        mut on_progress: F,
    ) -> Result<PathBuf>
    where
        F: FnMut(DownloadProgress) + Send + 'static,
    {
        tokio::fs::create_dir_all(temp_dir).await?;

        // Sanitize slug for filesystem safe names
        let safe_slug: String = slug
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
            .collect();

        let out_template = temp_dir
            .join(format!("{}_{}_download.%(ext)s", job_id, safe_slug))
            .to_string_lossy()
            .to_string();

        let mut cmd = Command::new(&self.ytdl_path);
        cmd.args([
            "-f",
            "bestvideo[ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best",
            "--outtmpl",
            &out_template,
            "--newline",
            "--no-check-certificates",
            "--no-warnings",
        ]);

        if let Some(ref_url) = referer {
            cmd.arg("--add-header").arg(format!("Referer: {}", ref_url));
        }
        if let Some(ua) = user_agent {
            cmd.arg("--user-agent").arg(ua);
        }
        if let Some(cookie_str) = cookies {
            cmd.arg("--add-header").arg(format!("Cookie: {}", cookie_str));
        }

        cmd.arg(url);

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        info!("Starting yt-dlp download for Job #{} ({})...", job_id, slug);
        let mut child = cmd.spawn().with_context(|| format!("Failed to spawn yt-dlp at {:?}", self.ytdl_path))?;

        let stdout = child.stdout.take().ok_or_else(|| anyhow!("Failed to capture stdout"))?;
        let mut reader = BufReader::new(stdout).lines();

        // Regex for yt-dlp: [download]  45.2% of ~  15.20MiB at  2.45MiB/s ETA 00:04
        let progress_regex = Regex::new(r"\[download\]\s+([\d\.]+)%\s+of.*?at\s+([^\s]+)\s+ETA\s+([^\s]+)")
            .expect("Valid regex");

        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(caps) = progress_regex.captures(&line) {
                let percent: f64 = caps[1].parse().unwrap_or(0.0);
                let speed = caps[2].to_string();
                let eta = caps[3].to_string();

                on_progress(DownloadProgress {
                    percent,
                    speed,
                    eta,
                });
            }
        }

        let status = child.wait().await?;
        if !status.success() {
            return Err(anyhow!("yt-dlp exited with non-zero status: {:?}", status.code()));
        }

        // Find the actual output file in temp_dir matching the job_id prefix
        let prefix = format!("{}_{}_download.", job_id, safe_slug);
        let mut dir = tokio::fs::read_dir(temp_dir).await?;
        while let Some(entry) = dir.next_entry().await? {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with(&prefix) && !fname.ends_with(".tmp") && !fname.ends_with(".part") {
                return Ok(entry.path());
            }
        }

        Err(anyhow!("Downloaded file not found in temp directory for Job #{}", job_id))
    }
}
