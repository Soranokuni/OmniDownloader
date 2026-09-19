use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;

pub struct WatchfolderDelivery;

impl WatchfolderDelivery {
    pub async fn deliver(
        source_file: &Path,
        watchfolder_dir: &Path,
        slug: &str,
    ) -> Result<PathBuf> {
        tokio::fs::create_dir_all(watchfolder_dir).await?;

        let target_filename = format!("{}.mxf", slug);
        let final_dest = watchfolder_dir.join(&target_filename);

        // Try direct rename first (atomic if on the same drive)
        if tokio::fs::rename(source_file, &final_dest).await.is_ok() {
            info!("Atomic direct rename delivery complete: {:?}", final_dest);
            return Ok(final_dest);
        }

        // Cross-drive or network share fallback: copy to hidden temporary file in target directory first
        let temp_filename = format!(".{}.mxf.tmp", slug);
        let temp_dest = watchfolder_dir.join(&temp_filename);

        if temp_dest.exists() {
            let _ = tokio::fs::remove_file(&temp_dest).await;
        }

        tokio::fs::copy(source_file, &temp_dest)
            .await
            .with_context(|| format!("Failed copying to temporary watchfolder file {:?}", temp_dest))?;

        // Same directory rename is guaranteed atomic
        if final_dest.exists() {
            let _ = tokio::fs::remove_file(&final_dest).await;
        }
        tokio::fs::rename(&temp_dest, &final_dest)
            .await
            .with_context(|| format!("Failed atomic rename to {:?}", final_dest))?;

        // Clean up source file
        let _ = tokio::fs::remove_file(source_file).await;

        info!("Atomic cross-volume watchfolder delivery complete: {:?}", final_dest);
        Ok(final_dest)
    }

    pub async fn cleanup_job_temp_files(temp_dir: &Path, job_id: i64) {
        if let Ok(mut dir) = tokio::fs::read_dir(temp_dir).await {
            let prefix = format!("{}_", job_id);
            while let Ok(Some(entry)) = dir.next_entry().await {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.starts_with(&prefix) {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_atomic_watchfolder_delivery_and_cleanup() -> Result<()> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("omni_test_delivery_{}", nanos));

        let watchfolder_dir = temp_dir.join("watchfolder");
        let work_dir = temp_dir.join("work");

        tokio::fs::create_dir_all(&watchfolder_dir).await?;
        tokio::fs::create_dir_all(&work_dir).await?;

        // 1. Create a dummy MXF in work_dir
        let source_file = work_dir.join("temp_interm.mxf");
        tokio::fs::write(&source_file, b"MOCK_BROADCAST_MXF_BYTES").await?;

        // 2. Deliver atomically into watchfolder
        let delivered_path = WatchfolderDelivery::deliver(&source_file, &watchfolder_dir, "1_PAPADAKI_TOPIC").await?;
        assert!(delivered_path.exists());
        assert_eq!(delivered_path.file_name().unwrap(), "1_PAPADAKI_TOPIC.mxf");
        assert!(!source_file.exists()); // Source must have been moved/cleaned

        // 3. Test cleanup_job_temp_files
        let dummy_job_temp = work_dir.join("999_intermediate.wav");
        tokio::fs::write(&dummy_job_temp, b"pcm_audio").await?;
        assert!(dummy_job_temp.exists());

        WatchfolderDelivery::cleanup_job_temp_files(&work_dir, 999).await;
        assert!(!dummy_job_temp.exists());

        // Cleanup test directory
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        Ok(())
    }
}

