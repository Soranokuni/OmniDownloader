use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::info;

pub struct Rewrapper {
    bmxtranswrap_path: PathBuf,
}

impl Rewrapper {
    pub fn new<P: AsRef<Path>>(bmxtranswrap_path: P) -> Self {
        Self {
            bmxtranswrap_path: bmxtranswrap_path.as_ref().to_path_buf(),
        }
    }

    pub async fn rewrap(&self, job_id: i64, intermediate_mxf: &Path, temp_dir: &Path) -> Result<PathBuf> {
        tokio::fs::create_dir_all(temp_dir).await?;
        let out_filename = format!("{}_output.mxf", job_id);
        let final_temp_mxf = temp_dir.join(&out_filename);

        if final_temp_mxf.exists() {
            let _ = tokio::fs::remove_file(&final_temp_mxf).await;
        }

        let mut cmd = Command::new(&self.bmxtranswrap_path);
        cmd.args([
            "-t",
            "rdd9",
            "--tc-rate",
            "25",
            "-o",
            final_temp_mxf.to_string_lossy().as_ref(),
            intermediate_mxf.to_string_lossy().as_ref(),
        ]);

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        info!("Running bmxtranswrap SMPTE RDD9 re-wrap for Job #{}...", job_id);
        let status = cmd
            .status()
            .await
            .with_context(|| format!("Failed running bmxtranswrap at {:?}", self.bmxtranswrap_path))?;

        if !status.success() || !final_temp_mxf.exists() {
            return Err(anyhow!("bmxtranswrap failed with exit code: {:?}", status.code()));
        }

        Ok(final_temp_mxf)
    }
}
