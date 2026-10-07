//! Atomic delivery into the Dalet Galaxy watchfolder (plan P1.4, P1.7).
//!
//! Dalet scans the watchfolder every few seconds and ingests whatever it finds.
//! Two properties follow from that, and both were broken:
//!
//! * **A file must never appear under its final name until it is complete.**
//!   Dalet will happily ingest a half-written MXF. The file is written to a
//!   hidden `.{slug}.mxf.tmp` *inside the destination directory*, flushed to
//!   disk, size-checked, and only then renamed — a same-directory rename is
//!   atomic, including on an SMB share.
//! * **An existing file is never deleted or overwritten** (defect D-05). A slug
//!   collision used to silently replace an asset Dalet had already ingested and
//!   an editor may already have cut into a running order. Collisions now get a
//!   `_2`, `_3` suffix and the job records what it actually delivered.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

/// Result of a delivery, so the job row can record what really landed.
#[derive(Debug, Clone)]
pub struct Delivered {
    pub path: PathBuf,
    /// Final filename, which may carry a collision suffix.
    pub filename: String,
    /// True when the slug collided and a `_N` suffix was used.
    pub suffixed: bool,
    pub bytes: u64,
}

/// Copy `source` to `dest` and flush `dest` to the device; the size on disk.
///
/// The copy is the operating system's (`CopyFileExW` on Windows: large
/// pipelined I/O, server-side copy on an SMB share). It replaces a
/// `tokio::io::copy`, whose 8 KiB chunks each went through the blocking pool
/// — a 5.8 GB file (15 min at 50 Mbps) took 66 s to deliver.
///
/// Flushed *before* the caller renames it. Without this the rename can be
/// visible to Dalet while the data is still in the write cache; a power cut
/// then leaves a correctly named, truncated file in the running order — the
/// worst possible failure mode here, because nothing downstream reports an
/// error. FlushFileBuffers needs write access, so the file is reopened for
/// writing (a read-only handle fails with ERROR_ACCESS_DENIED).
async fn copy_durably(source: &Path, dest: &Path) -> Result<u64> {
    let (source, dest) = (source.to_path_buf(), dest.to_path_buf());
    tokio::task::spawn_blocking(move || -> Result<u64> {
        std::fs::copy(&source, &dest).with_context(|| format!("Failed copying {source:?} to {dest:?}"))?;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&dest)
            .with_context(|| format!("Failed reopening {dest:?} to flush it"))?;
        f.sync_all().with_context(|| format!("Failed flushing {dest:?} to disk"))?;
        Ok(f.metadata().with_context(|| format!("Failed reading the size of {dest:?}"))?.len())
    })
    .await
    .context("delivery copy task")?
}

pub struct WatchfolderDelivery;

impl WatchfolderDelivery {
    /// Deliver `source_file` into `watchfolder_dir` as `{slug}.mxf`.
    ///
    /// On a name collision the file is delivered as `{slug}_2.mxf`, `_3`, … The
    /// caller is expected to surface `suffixed` to MCR so an operator knows two
    /// assets now share a slug.
    pub async fn deliver(
        source_file: &Path,
        watchfolder_dir: &Path,
        slug: &str,
    ) -> Result<Delivered> {
        tokio::fs::create_dir_all(watchfolder_dir)
            .await
            .with_context(|| format!("Failed creating watchfolder {watchfolder_dir:?}"))?;

        let source_bytes = tokio::fs::metadata(source_file)
            .await
            .with_context(|| format!("Delivery source {source_file:?} is not readable"))?
            .len();
        if source_bytes == 0 {
            return Err(anyhow!(
                "Refusing to deliver a zero-byte file for slug {slug}"
            ));
        }

        let (final_dest, filename, suffixed) = Self::pick_free_name(watchfolder_dir, slug).await?;

        // Always stage through a hidden temporary file in the *destination*
        // directory. The same-volume rename shortcut is deliberately not used:
        // the copy path is the one that has to be correct on an SMB share, so it
        // is the only path, and it is exercised on every delivery rather than
        // only when the watchfolder happens to be remote.
        let temp_dest = watchfolder_dir.join(format!(".{filename}.tmp"));
        if temp_dest.exists() {
            // Our own leftover from a crashed delivery: the dot-prefixed name is
            // ours by convention and Dalet ignores it.
            let _ = tokio::fs::remove_file(&temp_dest).await;
        }

        let written = match copy_durably(source_file, &temp_dest).await {
            Ok(n) => n,
            Err(e) => {
                let _ = tokio::fs::remove_file(&temp_dest).await;
                return Err(e);
            }
        };
        if written != source_bytes {
            let _ = tokio::fs::remove_file(&temp_dest).await;
            return Err(anyhow!(
                "Short write delivering {slug}: {written} bytes written, {source_bytes} expected"
            ));
        }

        tokio::fs::rename(&temp_dest, &final_dest)
            .await
            .with_context(|| format!("Failed atomic rename to {final_dest:?}"))?;

        if suffixed {
            warn!(
                "Slug collision: delivered as {filename} because {slug}.mxf already exists in the watchfolder"
            );
        }
        info!("Delivered {} ({} bytes) to {:?}", filename, written, final_dest);

        Ok(Delivered {
            path: final_dest,
            filename,
            suffixed,
            bytes: written,
        })
    }

    /// First free `{slug}.mxf`, `{slug}_2.mxf`, … in the destination.
    ///
    /// Also treats a live `.{name}.tmp` as taken, so two workers delivering
    /// colliding slugs at the same moment do not stage into the same temporary
    /// file and corrupt each other.
    async fn pick_free_name(dir: &Path, slug: &str) -> Result<(PathBuf, String, bool)> {
        for n in 1..=999u32 {
            let filename = if n == 1 {
                format!("{slug}.mxf")
            } else {
                format!("{slug}_{n}.mxf")
            };
            let dest = dir.join(&filename);
            let staging = dir.join(format!(".{filename}.tmp"));
            if !dest.exists() && !staging.exists() {
                return Ok((dest, filename, n > 1));
            }
        }
        Err(anyhow!(
            "Refusing to deliver {slug}: 999 files with that slug already exist in the watchfolder"
        ))
    }

    /// Remove one job's scratch directory (plan P1.4, defect D-04).
    ///
    /// The previous implementation deleted every file in a shared temp
    /// directory whose name started with `"{job_id}_"`. Job 1 therefore deleted
    /// the working files of jobs 10-19, 100-199 and 1000-1999 while they were
    /// mid-transcode — concurrent jobs silently corrupted each other, and the
    /// symptom (ffmpeg failing on a vanished input) pointed nowhere near the
    /// cause.
    ///
    /// Each job now owns `temp/jobs/{id}/` and cleanup removes exactly that
    /// directory. Never reintroduce a filename-prefix match here.
    pub async fn cleanup_job_temp_files(job_temp_dir: &Path) {
        if !job_temp_dir.exists() {
            return;
        }
        if let Err(e) = tokio::fs::remove_dir_all(job_temp_dir).await {
            // A tool still holding a handle is the usual cause. Worth a warning,
            // never worth failing a delivered job over.
            warn!("Could not remove job temp directory {job_temp_dir:?}: {e}");
        }
    }

    /// Remove scratch directories for jobs that are no longer running.
    ///
    /// Called at start-up: a crash leaves per-job directories behind, and on a
    /// busy newsroom machine those are gigabytes of source media.
    pub async fn sweep_orphan_job_dirs(jobs_root: &Path, running: &[i64]) -> usize {
        let mut removed = 0;
        let Ok(mut dir) = tokio::fs::read_dir(jobs_root).await else {
            return 0;
        };
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            // Only touch directories whose name is a job id. Anything else in
            // there was put there by a human and is not ours to delete.
            let Ok(id) = name.parse::<i64>() else {
                continue;
            };
            if running.contains(&id) {
                continue;
            }
            if tokio::fs::remove_dir_all(entry.path()).await.is_ok() {
                removed += 1;
            }
        }
        if removed > 0 {
            info!("Removed {removed} orphaned job temp director(ies)");
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn scratch() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let watchfolder = dir.path().join("watchfolder");
        let work = dir.path().join("work");
        tokio::fs::create_dir_all(&watchfolder).await.unwrap();
        tokio::fs::create_dir_all(&work).await.unwrap();
        (dir, watchfolder, work)
    }

    async fn source(work: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = work.join(name);
        tokio::fs::write(&p, content).await.unwrap();
        p
    }

    #[tokio::test]
    async fn delivers_under_the_slug_and_leaves_no_temporary_file() {
        let (_d, watchfolder, work) = scratch().await;
        let src = source(&work, "out.mxf", b"MOCK_BROADCAST_MXF_BYTES").await;

        let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "1_PAPADAKI_TOPIC")
            .await
            .unwrap();

        assert_eq!(delivered.filename, "1_PAPADAKI_TOPIC.mxf");
        assert!(!delivered.suffixed);
        assert!(delivered.path.exists());
        assert_eq!(delivered.bytes, b"MOCK_BROADCAST_MXF_BYTES".len() as u64);

        // Nothing hidden left behind for Dalet's scanner to trip over.
        let leftovers: Vec<String> = std::fs::read_dir(&watchfolder)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.') || n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "staging files left behind: {leftovers:?}");
    }

    #[tokio::test]
    async fn a_large_file_arrives_byte_for_byte_and_quickly() {
        // P1.11: the OS copy replaced an 8 KiB-chunk tokio copy that
        // delivered at ~90 MB/s. 48 MiB of non-repeating bytes, so a copy
        // that dropped or reordered a block cannot pass.
        let (_d, watchfolder, work) = scratch().await;
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let content: Vec<u8> = (0..48 * 1024 * 1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let src = source(&work, "big.mxf", &content).await;

        let started = std::time::Instant::now();
        let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "2_NIKOLAOU_BIG").await.unwrap();
        let took = started.elapsed();

        assert_eq!(delivered.bytes, content.len() as u64);
        assert!(std::fs::read(&delivered.path).unwrap() == content, "delivered bytes differ from the source");
        // Generous (the old copy needed ~0.5 s here, an OS copy ~0.05 s):
        // only a return to per-chunk round trips on a slow disk trips it.
        assert!(took < std::time::Duration::from_secs(5), "delivery of 48 MiB took {took:?}");
    }

    #[tokio::test]
    async fn an_existing_watchfolder_file_is_never_overwritten() {
        // Defect D-05. The old code called remove_file on the destination, so a
        // slug collision silently replaced an asset Dalet had already ingested
        // and an editor may already have cut into a running order.
        let (_d, watchfolder, work) = scratch().await;

        let already_on_air = watchfolder.join("3_PAPADAKI_KNICKS.mxf");
        tokio::fs::write(&already_on_air, b"THE_ASSET_ALREADY_IN_DALET")
            .await
            .unwrap();

        let src = source(&work, "new.mxf", b"A_DIFFERENT_CLIP").await;
        let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "3_PAPADAKI_KNICKS")
            .await
            .unwrap();

        assert_eq!(delivered.filename, "3_PAPADAKI_KNICKS_2.mxf");
        assert!(delivered.suffixed, "collision was not flagged to the operator");

        // The original is byte-for-byte untouched.
        assert_eq!(
            tokio::fs::read(&already_on_air).await.unwrap(),
            b"THE_ASSET_ALREADY_IN_DALET",
            "an asset already in the watchfolder was overwritten"
        );
    }

    #[tokio::test]
    async fn repeated_collisions_keep_counting_up() {
        let (_d, watchfolder, work) = scratch().await;
        for n in 1..=3 {
            let src = source(&work, &format!("clip{n}.mxf"), format!("clip{n}").as_bytes()).await;
            let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "1_MCR_X")
                .await
                .unwrap();
            let expected = if n == 1 {
                "1_MCR_X.mxf".to_string()
            } else {
                format!("1_MCR_X_{n}.mxf")
            };
            assert_eq!(delivered.filename, expected);
        }
        let count = std::fs::read_dir(&watchfolder).unwrap().count();
        assert_eq!(count, 3, "deliveries replaced each other instead of coexisting");
    }

    #[tokio::test]
    async fn a_zero_byte_source_is_refused() {
        // Better a job in review than a zero-byte file in the running order.
        let (_d, watchfolder, work) = scratch().await;
        let src = source(&work, "empty.mxf", b"").await;
        let err = WatchfolderDelivery::deliver(&src, &watchfolder, "1_MCR_EMPTY")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("zero-byte"), "{err}");
        assert_eq!(
            std::fs::read_dir(&watchfolder).unwrap().count(),
            0,
            "a zero-byte delivery still produced a file"
        );
    }

    #[tokio::test]
    async fn cleanup_removes_only_the_given_jobs_directory() {
        // Defect D-04: cleanup used to match the filename prefix "{id}_", so
        // job 1 deleted the working files of jobs 10-19 and 100-199 mid-flight.
        let (_d, _watchfolder, work) = scratch().await;
        let jobs = work.join("jobs");

        for id in [1i64, 10, 19, 100] {
            let dir = jobs.join(id.to_string());
            tokio::fs::create_dir_all(&dir).await.unwrap();
            tokio::fs::write(dir.join("source.mp4"), b"media").await.unwrap();
        }

        WatchfolderDelivery::cleanup_job_temp_files(&jobs.join("1")).await;

        assert!(!jobs.join("1").exists(), "job 1's directory was not removed");
        for survivor in [10i64, 19, 100] {
            assert!(
                jobs.join(survivor.to_string()).join("source.mp4").exists(),
                "cleaning up job 1 destroyed job {survivor}'s working files"
            );
        }
    }

    #[tokio::test]
    async fn the_startup_sweep_spares_running_jobs_and_foreign_directories() {
        let (_d, _watchfolder, work) = scratch().await;
        let jobs = work.join("jobs");
        for name in ["1", "2", "3", "notes-from-an-engineer"] {
            let dir = jobs.join(name);
            tokio::fs::create_dir_all(&dir).await.unwrap();
            tokio::fs::write(dir.join("f"), b"x").await.unwrap();
        }

        let removed = WatchfolderDelivery::sweep_orphan_job_dirs(&jobs, &[2]).await;

        assert_eq!(removed, 2, "expected jobs 1 and 3 to be swept");
        assert!(!jobs.join("1").exists());
        assert!(jobs.join("2").exists(), "a running job's workspace was deleted");
        assert!(!jobs.join("3").exists());
        assert!(
            jobs.join("notes-from-an-engineer").exists(),
            "the sweep deleted a directory that is not a job id"
        );
    }
}
