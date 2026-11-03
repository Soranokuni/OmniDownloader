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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

/// A staging file (`.{name}.mxf.tmp`) or start-up probe untouched for this long
/// is a leftover from an interrupted copy, not a copy in progress (P1.22).
pub const STALE_STAGING_AGE: Duration = Duration::from_secs(10 * 60);

/// Staging paths this process is writing right now. A path in here is never
/// reclaimed, whatever its age.
fn in_flight() -> &'static Mutex<HashSet<PathBuf>> {
    static REGISTRY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Holds one staging path in the in-flight registry; removed on drop, so every
/// exit path of a delivery (error, early return, cancelled future) releases it.
struct StagingClaim(PathBuf);

impl StagingClaim {
    /// `None` when another delivery in this process already holds `path`.
    fn try_new(path: PathBuf) -> Option<Self> {
        let mut set = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        set.insert(path.clone()).then(|| Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StagingClaim {
    fn drop(&mut self) {
        in_flight().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// Returned by [`WatchfolderDelivery::deliver_with`] when the caller's hook said
/// the job is no longer ours: nothing reached the final name.
#[derive(Debug)]
pub struct LeaseLostBeforeRename;

impl std::fmt::Display for LeaseLostBeforeRename {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the job's lease was lost before the file was renamed into the watchfolder")
    }
}

impl std::error::Error for LeaseLostBeforeRename {}

/// Exactly `.<name>.mxf.tmp`: the staging name this module writes.
fn is_staging_name(name: &str) -> bool {
    name.len() > ".mxf.tmp".len() + 1 && name.starts_with('.') && name.ends_with(".mxf.tmp")
}

/// `.omni-probe-<pid>.tmp`: the writability probe `selftest` leaves if it dies.
fn is_probe_name(name: &str) -> bool {
    name.strip_prefix(".omni-probe-")
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

/// Remove `path` if it is a leftover: old enough and not open anywhere. The
/// caller has checked the name and the in-flight registry. Never removes
/// anything it could not open exclusively.
fn remove_if_stale(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    // The newer of last-write and creation: a copy can carry its source's old
    // last-write time (`copy_durably` resets it, but a file from elsewhere may not).
    let last_touched = [meta.modified().ok(), meta.created().ok()].into_iter().flatten().max();
    let old_enough = last_touched
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age >= STALE_STAGING_AGE);
    if !old_enough {
        return false;
    }
    // Exclusive open: fails when any handle (another daemon, a copy still
    // running, Dalet) has the file open. Closed again before the removal.
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
    }
    match opts.open(path) {
        Ok(handle) => drop(handle),
        Err(_) => return false,
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            info!(
                "Removed stale staging file {}",
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            );
            true
        }
        Err(_) => false,
    }
}

/// A rename that returned an error nevertheless happened when the final file is
/// there with exactly the bytes written and the staging file is gone.
fn rename_landed(final_len: Option<u64>, written: u64, staging_exists: bool) -> bool {
    final_len == Some(written) && !staging_exists
}

/// Remove the leftovers of interrupted copies and probes from the top level of
/// the watchfolder (P1.22). Only `.<name>.mxf.tmp` and `.omni-probe-<pid>.tmp`
/// files, only when old enough, not being written by this process and not open
/// anywhere. A finished `.mxf` and every other file are never touched. Returns
/// how many were removed.
pub fn sweep_stale_staging(watchfolder: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(watchfolder) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = is_staging_name(&name) || is_probe_name(&name);
        if !ours || !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        // Claimed for the removal, so a delivery that picks this name now waits
        // for the next name instead of staging into a file being deleted.
        let Some(claim) = StagingClaim::try_new(entry.path()) else {
            continue;
        };
        if remove_if_stale(claim.path()) {
            removed += 1;
        }
    }
    removed
}

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
        // CopyFileExW gives the copy its source's last-write time: an hours-old held
        // file would look stale from the first byte, and another instance on the
        // share could reclaim the staging file before the rename. Written now.
        f.set_modified(SystemTime::now())
            .with_context(|| format!("Failed stamping the time on {dest:?}"))?;
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
        Self::deliver_with(source_file, watchfolder_dir, slug, |_, _| Ok(true)).await
    }

    /// [`Self::deliver`] with a hook that runs after the staging file is
    /// written, flushed and size-checked, immediately before the rename. It gets
    /// the final path and size. `Ok(true)` goes on; `Ok(false)` abandons the
    /// delivery with [`LeaseLostBeforeRename`]; `Err` abandons it with that
    /// error. Either way the staging file is removed and nothing is renamed.
    /// The hook runs again if the name has to change because something took it
    /// during the copy, so it always describes the path about to be renamed to.
    pub async fn deliver_with<F>(
        source_file: &Path,
        watchfolder_dir: &Path,
        slug: &str,
        mut before_rename: F,
    ) -> Result<Delivered>
    where
        F: FnMut(&Path, u64) -> Result<bool>,
    {
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

        let (mut final_dest, mut filename, mut n, staged) = Self::pick_free_name(watchfolder_dir, slug).await?;

        // Always stage through a hidden temporary file in the *destination*
        // directory. The same-volume rename shortcut is deliberately not used:
        // the copy path is the one that has to be correct on an SMB share, so it
        // is the only path, and it is exercised on every delivery rather than
        // only when the watchfolder happens to be remote.
        //
        // `staged` holds the staging path in the in-flight registry until this
        // function returns, however it returns, so neither the sweep nor another
        // worker's name pick can mistake a copy in progress for a stale leftover.
        let temp_dest = staged.path().to_path_buf();

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

        // The last moment before the file can be seen under its final name: the
        // caller records where it is going, so a crash after the rename is
        // recognised at the next start-up and the video is not delivered twice.
        loop {
            match before_rename(&final_dest, written) {
                Ok(true) => {}
                Ok(false) => {
                    let _ = tokio::fs::remove_file(&temp_dest).await;
                    return Err(anyhow::Error::new(LeaseLostBeforeRename));
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&temp_dest).await;
                    return Err(e);
                }
            }

            // Never replaces: on Windows a plain rename silently overwrites an
            // existing target, and a file that appeared during the copy (another
            // instance, a person) must survive (defect D-05).
            let (from, to) = (temp_dest.clone(), final_dest.clone());
            let renamed = tokio::task::spawn_blocking(move || omni_core::fsutil::rename_no_replace(&from, &to))
                .await
                .context("delivery rename task")?;
            match renamed {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Taken since the pick: the next free name, same staging file.
                    let taken = filename.clone();
                    let next = Self::next_free_after(watchfolder_dir, slug, n);
                    let Some((next_n, next_name)) = next else {
                        let _ = tokio::fs::remove_file(&temp_dest).await;
                        return Err(anyhow!(
                            "Refusing to deliver {slug}: 999 files with that slug already exist in the watchfolder"
                        ));
                    };
                    warn!("{taken} appeared in the watchfolder during the copy; delivering as {next_name}");
                    (n, filename) = (next_n, next_name);
                    final_dest = watchfolder_dir.join(&filename);
                }
                Err(e) => {
                    // An SMB rename can succeed while its reply is lost: then the file
                    // is there under its final name and the staging file is gone, and
                    // delivering again would put the video on air twice.
                    let final_len = std::fs::metadata(&final_dest).ok().filter(|m| m.is_file()).map(|m| m.len());
                    if rename_landed(final_len, written, temp_dest.exists()) {
                        warn!("The rename to {filename} reported an error ({e}) but the file arrived complete; treating it as delivered");
                        break;
                    }
                    return Err(anyhow::Error::new(e).context(format!("Failed atomic rename to {final_dest:?}")));
                }
            }
        }
        drop(staged);
        let suffixed = n > 1;

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
    /// file and corrupt each other. The staging path is claimed in the in-flight
    /// registry as part of the pick, so the choice itself is race-free. A staging
    /// file that is old, unopened and not in flight is a leftover of an
    /// interrupted copy and is reclaimed (P1.22) instead of costing a `_N`.
    async fn pick_free_name(dir: &Path, slug: &str) -> Result<(PathBuf, String, u32, StagingClaim)> {
        for n in 1..=999u32 {
            let filename = if n == 1 {
                format!("{slug}.mxf")
            } else {
                format!("{slug}_{n}.mxf")
            };
            let dest = dir.join(&filename);
            let Some(claim) = StagingClaim::try_new(dir.join(format!(".{filename}.tmp"))) else {
                continue;
            };
            if dest.exists() {
                continue;
            }
            if claim.path().exists() && !remove_if_stale(claim.path()) {
                continue;
            }
            return Ok((dest, filename, n, claim));
        }
        Err(anyhow!(
            "Refusing to deliver {slug}: 999 files with that slug already exist in the watchfolder"
        ))
    }

    /// The first name after number `after` whose final file does not exist.
    fn next_free_after(dir: &Path, slug: &str, after: u32) -> Option<(u32, String)> {
        (after + 1..=999u32)
            .map(|n| (n, format!("{slug}_{n}.mxf")))
            .find(|(_, name)| !dir.join(name).exists())
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

    // ---- P1.22: stale staging files ----

    /// Make `path` look like it was last written `secs` ago.
    fn age(path: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let then = SystemTime::now() - Duration::from_secs(secs);
        let times = std::fs::FileTimes::new().set_modified(then);
        // A real leftover is old in both senses; the sweep goes by the newer.
        #[cfg(windows)]
        let times = {
            use std::os::windows::fs::FileTimesExt;
            times.set_created(then)
        };
        f.set_times(times).unwrap();
    }

    fn old_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        age(&p, 3600);
        p
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn an_old_unheld_staging_file_is_reclaimed_and_the_name_is_reused() {
        let (_d, watchfolder, work) = scratch().await;
        let stale = old_file(&watchfolder, ".5_ANNA_CLIP.mxf.tmp", b"half a copy from a crash");
        let src = source(&work, "out.mxf", b"THE_REAL_VIDEO").await;

        let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "5_ANNA_CLIP").await.unwrap();

        assert_eq!(delivered.filename, "5_ANNA_CLIP.mxf", "a stale staging file cost a _N");
        assert!(!delivered.suffixed);
        assert!(!stale.exists());
        assert_eq!(names(&watchfolder), vec!["5_ANNA_CLIP.mxf".to_string()]);
        assert_eq!(std::fs::read(&delivered.path).unwrap(), b"THE_REAL_VIDEO");
    }

    #[tokio::test]
    async fn a_fresh_staging_file_is_kept_and_the_next_name_is_used() {
        let (_d, watchfolder, work) = scratch().await;
        let fresh = watchfolder.join(".6_ANNA_CLIP.mxf.tmp");
        std::fs::write(&fresh, b"another daemon is copying").unwrap();
        age(&fresh, 60);
        let src = source(&work, "out.mxf", b"VIDEO").await;

        let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "6_ANNA_CLIP").await.unwrap();

        assert_eq!(delivered.filename, "6_ANNA_CLIP_2.mxf");
        assert_eq!(std::fs::read(&fresh).unwrap(), b"another daemon is copying", "a fresh staging file was touched");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn an_old_staging_file_another_handle_holds_open_is_kept() {
        let (_d, watchfolder, work) = scratch().await;
        let held = old_file(&watchfolder, ".7_ANNA_CLIP.mxf.tmp", b"still being read or written");
        let _other_handle = std::fs::File::open(&held).unwrap();
        let src = source(&work, "out.mxf", b"VIDEO").await;

        let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "7_ANNA_CLIP").await.unwrap();

        assert_eq!(delivered.filename, "7_ANNA_CLIP_2.mxf");
        assert!(held.exists(), "a staging file another process had open was removed");
        assert_eq!(sweep_stale_staging(&watchfolder), 0, "the sweep removed a file that was open");
        assert!(held.exists());
    }

    #[tokio::test]
    async fn a_staging_file_in_the_in_flight_registry_is_kept_however_old() {
        let (_d, watchfolder, work) = scratch().await;
        let busy = old_file(&watchfolder, ".8_ANNA_CLIP.mxf.tmp", b"this process is writing it");
        let claim = StagingClaim::try_new(busy.clone()).expect("claimed");
        let src = source(&work, "out.mxf", b"VIDEO").await;

        let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "8_ANNA_CLIP").await.unwrap();
        assert_eq!(delivered.filename, "8_ANNA_CLIP_2.mxf");
        assert_eq!(sweep_stale_staging(&watchfolder), 0);
        assert!(busy.exists(), "an in-flight staging file was removed");

        // Released on drop: now it is just an old leftover.
        drop(claim);
        assert_eq!(sweep_stale_staging(&watchfolder), 1);
        assert!(!busy.exists());
    }

    #[tokio::test]
    async fn files_that_are_not_ours_and_finished_mxf_files_are_never_touched() {
        let (_d, watchfolder, work) = scratch().await;
        let bystanders = [
            ("9_ANNA_CLIP.mxf", &b"already ingested by Dalet"[..]),
            ("notes.txt", b"an editor's note"),
            (".hidden.tmp", b"not our suffix"),
            ("9_ANNA_CLIP.mxf.tmp", b"no leading dot"),
            (".9_ANNA_CLIP.mxf", b"no .tmp"),
            (".mxf.tmp", b"no name between"),
            (".omni-probe-abc.tmp", b"not a pid"),
            (".omni-probe-.tmp", b"empty pid"),
            (".other-tool-123.tmp", b"somebody else's probe"),
        ];
        for (name, content) in bystanders {
            old_file(&watchfolder, name, content);
        }
        std::fs::create_dir(watchfolder.join("sub")).unwrap();
        old_file(&watchfolder.join("sub"), ".nested.mxf.tmp", b"below the top level");
        let before = names(&watchfolder);
        let src = source(&work, "out.mxf", b"NEW").await;

        // The slug collides with the finished file: it is skipped, never replaced.
        let delivered = WatchfolderDelivery::deliver(&src, &watchfolder, "9_ANNA_CLIP").await.unwrap();
        assert_eq!(delivered.filename, "9_ANNA_CLIP_2.mxf");
        assert_eq!(sweep_stale_staging(&watchfolder), 0);

        let mut expected = before;
        expected.push("9_ANNA_CLIP_2.mxf".to_string());
        expected.sort();
        assert_eq!(names(&watchfolder), expected, "a file that is not ours was removed");
        for (name, content) in bystanders {
            assert_eq!(std::fs::read(watchfolder.join(name)).unwrap(), content, "{name} was changed");
        }
        assert!(watchfolder.join("sub").join(".nested.mxf.tmp").exists(), "the sweep recursed");
    }

    #[tokio::test]
    async fn the_sweep_removes_old_staging_and_probe_files_and_spares_fresh_ones() {
        let (_d, watchfolder, _work) = scratch().await;
        old_file(&watchfolder, ".1_OLD.mxf.tmp", b"x");
        old_file(&watchfolder, ".omni-probe-4242.tmp", b"omni");
        let fresh = watchfolder.join(".2_FRESH.mxf.tmp");
        std::fs::write(&fresh, b"y").unwrap();
        let fresh_probe = watchfolder.join(".omni-probe-99.tmp");
        std::fs::write(&fresh_probe, b"omni").unwrap();

        assert_eq!(sweep_stale_staging(&watchfolder), 2);
        assert_eq!(names(&watchfolder), vec![".2_FRESH.mxf.tmp".to_string(), ".omni-probe-99.tmp".to_string()]);
        assert_eq!(sweep_stale_staging(&watchfolder.join("does-not-exist")), 0);
    }

    #[tokio::test]
    async fn the_in_flight_claim_is_released_on_every_exit_path() {
        let (_d, watchfolder, work) = scratch().await;
        let src = source(&work, "out.mxf", b"VIDEO").await;
        let staging = watchfolder.join(".10_ANNA_CLIP.mxf.tmp");

        // Refused by the hook: nothing renamed, staging gone, claim released.
        let err = WatchfolderDelivery::deliver_with(&src, &watchfolder, "10_ANNA_CLIP", |_, _| Ok(false))
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<LeaseLostBeforeRename>().is_some(), "{err:#}");
        assert!(names(&watchfolder).is_empty(), "a refused delivery left files: {:?}", names(&watchfolder));
        assert!(StagingClaim::try_new(staging.clone()).is_some(), "claim still held after a refusal");

        // The hook failing: same.
        let err = WatchfolderDelivery::deliver_with(&src, &watchfolder, "10_ANNA_CLIP", |_, _| Err(anyhow!("db down")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("db down"));
        assert!(names(&watchfolder).is_empty());

        assert!(StagingClaim::try_new(staging).is_some(), "claim still held after a failing hook");

        // Success: the claim is released too.
        let ok = WatchfolderDelivery::deliver(&src, &watchfolder, "10_ANNA_CLIP").await.unwrap();
        assert_eq!(ok.filename, "10_ANNA_CLIP.mxf");
        assert!(StagingClaim::try_new(watchfolder.join(".10_ANNA_CLIP.mxf.tmp")).is_some());
    }

    #[tokio::test]
    async fn the_hook_runs_after_the_copy_and_before_the_final_name_exists() {
        let (_d, watchfolder, work) = scratch().await;
        let src = source(&work, "out.mxf", b"TWELVE_BYTES").await;
        let mut seen = None;
        let delivered = WatchfolderDelivery::deliver_with(&src, &watchfolder, "11_ANNA_CLIP", |to, bytes| {
            seen = Some((to.to_path_buf(), bytes, to.exists(), watchfolder.join(".11_ANNA_CLIP.mxf.tmp").exists()));
            Ok(true)
        })
        .await
        .unwrap();
        let (to, bytes, final_existed, staging_existed) = seen.expect("hook ran");
        assert_eq!((to, bytes), (delivered.path.clone(), 12));
        assert!(!final_existed, "the hook ran after the rename");
        assert!(staging_existed, "the hook ran before the copy finished");
    }

    #[tokio::test]
    async fn a_copy_of_an_old_source_is_not_stale_from_its_first_byte() {
        // CopyFileExW stamps the copy with the source's last-write time; an hours-old
        // held file must not make its own staging file reclaimable by another instance.
        let (_d, watchfolder, work) = scratch().await;
        let src = old_file(&work, "held.mxf", b"A_HELD_VIDEO_FROM_THIS_MORNING");
        let staging = watchfolder.join(".12_ANNA_CLIP.mxf.tmp");

        let n = copy_durably(&src, &staging).await.unwrap();

        assert_eq!(n, 30);
        let age_now = SystemTime::now().duration_since(std::fs::metadata(&staging).unwrap().modified().unwrap()).unwrap();
        assert!(age_now < Duration::from_secs(60), "the copy kept its source's last-write time ({age_now:?} old)");
        assert!(!remove_if_stale(&staging), "a staging file just written was judged stale");
        assert!(staging.exists());
        assert_eq!(sweep_stale_staging(&watchfolder), 0);
    }

    #[tokio::test]
    async fn a_file_with_an_old_last_write_time_but_a_fresh_creation_time_is_not_stale() {
        let (_d, watchfolder, _work) = scratch().await;
        let p = watchfolder.join(".14_ANNA_CLIP.mxf.tmp");
        std::fs::write(&p, b"just created, carries an old last-write time").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
        drop(f);

        assert!(!remove_if_stale(&p), "the newer of last-write and creation must decide");
        assert!(p.exists());
    }

    #[tokio::test]
    async fn a_name_taken_during_the_copy_is_never_replaced_and_the_next_name_is_used() {
        let (_d, watchfolder, work) = scratch().await;
        let src = source(&work, "out.mxf", b"THE_NEW_VIDEO").await;
        let mut seen: Vec<PathBuf> = Vec::new();
        let wf = watchfolder.clone();

        let delivered = WatchfolderDelivery::deliver_with(&src, &watchfolder, "13_ANNA_CLIP", |to, _| {
            if seen.is_empty() {
                // Something else creates the file between the pick and the rename.
                std::fs::write(wf.join("13_ANNA_CLIP.mxf"), b"CREATED_BY_SOMEONE_ELSE").unwrap();
            }
            seen.push(to.to_path_buf());
            Ok(true)
        })
        .await
        .unwrap();

        assert_eq!(std::fs::read(watchfolder.join("13_ANNA_CLIP.mxf")).unwrap(), b"CREATED_BY_SOMEONE_ELSE");
        assert_eq!(delivered.filename, "13_ANNA_CLIP_2.mxf");
        assert!(delivered.suffixed);
        assert_eq!(std::fs::read(&delivered.path).unwrap(), b"THE_NEW_VIDEO");
        assert_eq!(names(&watchfolder), vec!["13_ANNA_CLIP.mxf".to_string(), "13_ANNA_CLIP_2.mxf".to_string()]);
        // The hook was told about the name that was really used.
        assert_eq!(seen, vec![watchfolder.join("13_ANNA_CLIP.mxf"), watchfolder.join("13_ANNA_CLIP_2.mxf")]);
    }

    #[test]
    fn a_rename_that_errored_counts_as_delivered_only_when_the_file_is_there_whole_and_staging_is_gone() {
        assert!(rename_landed(Some(100), 100, false), "reply lost, file complete");
        assert!(!rename_landed(Some(100), 100, true), "staging still there: the rename did not happen");
        assert!(!rename_landed(Some(99), 100, false), "a different size is not our file");
        assert!(!rename_landed(None, 100, false), "nothing at the final name");
    }
}
