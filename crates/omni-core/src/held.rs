//! Videos held for MCR's approval (plan P10.5): where the file waits, how it
//! is fingerprinted, and the sweep that expires, supersedes and tidies.
//!
//! The file of job `{id}` lives alone in `temp/held/{id}/`. The directory is
//! removed as a whole, and the stray-directory pass only ever looks at names
//! that are whole numbers: never a prefix match (job 1 is not job 10).

use anyhow::Result;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::models::JobStatus;
use crate::repository::{held_sweep_action, HeldSweep, Repository};

/// `{temp}/held/{job_id}`; the same as [`crate::paths::AppPaths::held`].
pub fn held_dir(temp: &Path, job_id: i64) -> PathBuf {
    temp.join("held").join(job_id.to_string())
}

/// Lower-case hex SHA-256 of a file, streamed through an 8 MiB buffer.
/// Blocking: call from `spawn_blocking`.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    sha256_reader(std::fs::File::open(path)?)
}

/// Open `path` for reading so that nobody can write to, rename or delete it
/// while the handle lives (Windows: share mode `FILE_SHARE_READ` only). The
/// approved file is hashed through such a handle and copied while it is
/// still open, so what was hashed is what was delivered.
pub fn open_locked(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1); // FILE_SHARE_READ
    }
    options.open(path)
}

/// Lower-case hex SHA-256 of everything `file` yields from its current position.
pub fn sha256_reader(mut file: impl Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut hex = String::with_capacity(64);
    for b in hasher.finalize() {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}

/// Remove job `job_id`'s held directory, if there is one. Best effort: the
/// sweep tries again.
pub fn remove_held_dir(temp: &Path, job_id: i64) {
    let dir = held_dir(temp, job_id);
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            warn!("Job #{job_id}: could not remove the held directory {dir:?}: {e}");
        }
    }
}

/// The MCR desks connected to the live stream (P11.10), shared by the web
/// server, which counts them, and the reaper, which runs the held videos'
/// approval clocks only while one has been connected. Clones share one count.
#[derive(Debug, Clone, Default)]
pub struct DeskPresence {
    inner: std::sync::Arc<DeskInner>,
}

#[derive(Debug, Default)]
struct DeskInner {
    connected: std::sync::atomic::AtomicUsize,
    /// A desk was connected at some moment since the last [`DeskPresence::take_seen`].
    seen: std::sync::atomic::AtomicBool,
}

/// One connected desk; dropping it (the stream closed) disconnects it.
#[derive(Debug)]
pub struct DeskGuard(std::sync::Arc<DeskInner>);

impl Drop for DeskGuard {
    fn drop(&mut self) {
        self.0.connected.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl DeskPresence {
    pub fn new() -> Self {
        Self::default()
    }

    /// A desk connected; it stays connected until the guard is dropped.
    pub fn connect(&self) -> DeskGuard {
        self.inner.connected.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.seen.store(true, std::sync::atomic::Ordering::SeqCst);
        DeskGuard(self.inner.clone())
    }

    /// Desks connected now.
    pub fn connected(&self) -> usize {
        self.inner.connected.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether a desk has been connected since the last call: one is now, or
    /// one connected (however briefly) in between. Resets the "in between".
    pub fn take_seen(&self) -> bool {
        let between = self.inner.seen.swap(false, std::sync::atomic::Ordering::SeqCst);
        between || self.connected() > 0
    }
}

/// Run the approval clocks for one reaper tick (P11.10): the time since the
/// last tick counts against a held video only when a desk has been connected
/// over it; otherwise, and over a gap longer than `max_gap` (the daemon was
/// stopped), its expiry moves on. Call before [`sweep`].
pub fn run_clocks(repo: &Repository, desks: &DeskPresence, now: DateTime<Utc>, max_gap: chrono::Duration) -> Result<usize> {
    repo.account_held_clocks(now, desks.take_seen(), max_gap)
}

/// What one sweep did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HeldSweepReport {
    pub expired: Vec<i64>,
    pub superseded: Vec<i64>,
    pub forgotten: Vec<i64>,
    pub dirs_removed: usize,
}

impl HeldSweepReport {
    pub fn is_empty(&self) -> bool {
        self.expired.is_empty() && self.superseded.is_empty() && self.forgotten.is_empty() && self.dirs_removed == 0
    }
}

/// One pass over the held outputs (run from the reaper tick):
/// expire what waited too long, supersede what MCR moved on from, forget
/// rows of jobs that are gone, and remove `temp/held/{id}` directories that
/// no live row wants.
pub fn sweep(repo: &Repository, temp: &Path, now: DateTime<Utc>) -> Result<HeldSweepReport> {
    sweep_with(repo, temp, now, true)
}

/// The reaper's held-video step (P11.10b): run the approval clocks, then the
/// sweep. When the clocks could not be accounted, the sweep expires nothing
/// this tick (it still supersedes, forgets and tidies): an expiry against an
/// `expires_at` that was not moved on would count time no desk saw.
pub fn reaper_tick(
    repo: &Repository,
    temp: &Path,
    desks: &DeskPresence,
    now: DateTime<Utc>,
    max_gap: chrono::Duration,
) -> Result<HeldSweepReport> {
    sweep_after_clocks(repo, temp, now, run_clocks(repo, desks, now, max_gap))
}

/// [`reaper_tick`] once the clocks ran with result `clocks`.
pub fn sweep_after_clocks(repo: &Repository, temp: &Path, now: DateTime<Utc>, clocks: Result<usize>) -> Result<HeldSweepReport> {
    let expire = match clocks {
        Ok(_) => true,
        Err(e) => {
            warn!("Held-video clocks not accounted, nothing expires this time: {e:#}");
            false
        }
    };
    sweep_with(repo, temp, now, expire)
}

/// [`sweep`]; with `expire` false a row past its time is kept for a later pass.
fn sweep_with(repo: &Repository, temp: &Path, now: DateTime<Utc>, expire: bool) -> Result<HeldSweepReport> {
    let mut report = HeldSweepReport::default();
    for (row, status) in repo.list_live_held()? {
        let id = row.job_id;
        match held_sweep_action(row.decision.as_deref(), status, row.expires_at, now) {
            HeldSweep::Keep => {}
            HeldSweep::Expire if !expire => {}
            HeldSweep::Expire => {
                if repo.expire_held(id, now)? {
                    info!("Job #{id}: nobody approved the held video in time; expired");
                    remove_held_dir(temp, id);
                    report.expired.push(id);
                }
            }
            HeldSweep::Supersede => {
                if repo.supersede_held(id)? {
                    remove_held_dir(temp, id);
                    report.superseded.push(id);
                }
            }
            HeldSweep::Forget => {
                if repo.forget_held(id)? {
                    remove_held_dir(temp, id);
                    report.forgotten.push(id);
                }
            }
        }
    }
    report.dirs_removed = remove_stray_dirs(repo, temp)?;
    Ok(report)
}

/// Remove `temp/held/{id}` directories that nothing wants: no row, or a
/// row whose decision is final. Only directories named by a whole number
/// are looked at. A RUNNING job's directory is always left alone: its hold is
/// being (re)written and the row comes a moment after the file.
fn remove_stray_dirs(repo: &Repository, temp: &Path) -> Result<usize> {
    let Ok(entries) = std::fs::read_dir(temp.join("held")) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !path.is_dir() || name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(id) = name.parse::<i64>() else { continue };
        // Never while the job runs, whatever the row says: a retried job
        // writes its new hold into this very directory before it replaces
        // the old row.
        let running = repo.get_job(id)?.is_some_and(|j| j.status == JobStatus::Running);
        let wanted = running
            || match repo.get_held(id)? {
                Some(h) => matches!(h.decision.as_deref(), None | Some("approved")),
                None => false,
            };
        if !wanted && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NewJob;
    use crate::repository::DEFAULT_DEDUP_WINDOW_HOURS;
    use chrono::Duration;
    use tempfile::TempDir;

    fn repo(dir: &TempDir) -> Repository {
        Repository::new(dir.path().join("t.db")).unwrap()
    }

    /// P11.10: a desk counts while its guard lives; one that came and went
    /// between two ticks still counts as seen for that tick, not the next.
    #[test]
    fn a_desk_is_seen_while_connected_and_once_after_a_brief_visit() {
        let desks = DeskPresence::new();
        assert!(!desks.take_seen());
        let guard = desks.clone().connect();
        assert_eq!(desks.connected(), 1);
        assert!(desks.take_seen());
        assert!(desks.take_seen(), "still connected");
        drop(guard);
        assert_eq!(desks.connected(), 0);
        assert!(!desks.take_seen());
        drop(desks.connect());
        assert!(desks.take_seen(), "a brief visit between ticks counts");
        assert!(!desks.take_seen(), "and only once");
    }

    fn job(repo: &Repository, n: u32) -> i64 {
        repo.enqueue(
            &NewJob::new(&format!("https://example.gr/v{n}"), &format!("{n}_ANNA_TEST"), "ANNA"),
            DEFAULT_DEDUP_WINDOW_HOURS,
        )
        .unwrap()
        .job_id()
    }

    /// A job leased by "w" with a held file written to `temp/held/{id}`.
    fn held_job(repo: &Repository, temp: &Path, n: u32, expires_in: Duration) -> i64 {
        let id = job(repo, n);
        let leased = repo.lease_job("w", 180).unwrap().expect("leased");
        assert_eq!(leased.id, id);
        let dir = held_dir(temp, id);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mxf");
        std::fs::write(&file, format!("clip {n}")).unwrap();
        let sha = sha256_file(&file).unwrap();
        assert!(repo
            .hold_output(id, "w", &file.to_string_lossy(), 6, &sha, Some(10.0), Utc::now() + expires_in)
            .unwrap());
        assert!(repo.finish(id, "w", JobStatus::AwaitingApproval, None, None, None).unwrap());
        id
    }

    #[test]
    fn sha256_matches_the_known_digest() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("a.bin");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn pure_decisions() {
        use JobStatus::*;
        let now = Utc::now();
        let later = Some(now + Duration::hours(1));
        let past = Some(now - Duration::hours(1));
        let act = |d: Option<&str>, j: Option<JobStatus>, e| held_sweep_action(d, j, e, now);
        assert_eq!(act(None, Some(AwaitingApproval), later), HeldSweep::Keep);
        assert_eq!(act(None, Some(AwaitingApproval), past), HeldSweep::Expire);
        assert_eq!(act(None, Some(AwaitingApproval), None), HeldSweep::Expire, "unparseable expiry must not hold for ever");
        assert_eq!(act(None, Some(Running), past), HeldSweep::Keep, "the hold is being written");
        for s in [Pending, RequiresReview, Cancelled, Failed, Completed, CompletedManual, ManualDownload] {
            assert_eq!(act(None, Some(s), later), HeldSweep::Supersede, "{s:?}");
        }
        assert_eq!(act(Some("approved"), Some(Pending), later), HeldSweep::Keep);
        assert_eq!(act(Some("approved"), Some(Running), later), HeldSweep::Keep);
        for s in [AwaitingApproval, RequiresReview, Cancelled, Completed, CompletedManual] {
            assert_eq!(act(Some("approved"), Some(s), later), HeldSweep::Supersede, "{s:?}");
        }
        assert_eq!(act(None, None, later), HeldSweep::Forget);
        assert_eq!(act(Some("approved"), None, later), HeldSweep::Forget);
        assert_eq!(act(Some("delivered"), Some(Completed), past), HeldSweep::Keep);
    }

    #[test]
    fn expiry_sends_the_job_to_review_and_removes_the_directory() {
        let dir = TempDir::new().unwrap();
        let repo = repo(&dir);
        let id = held_job(&repo, dir.path(), 1, Duration::hours(24));
        let fresh = sweep(&repo, dir.path(), Utc::now()).unwrap();
        assert!(fresh.is_empty(), "{fresh:?}");
        assert!(held_dir(dir.path(), id).exists());

        let report = sweep(&repo, dir.path(), Utc::now() + Duration::hours(25)).unwrap();
        assert_eq!(report.expired, vec![id]);
        let j = repo.get_job(id).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::RequiresReview);
        assert_eq!(j.error_code.as_deref(), Some("APPROVAL_EXPIRED"));
        assert_eq!(repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("expired"));
        assert!(!held_dir(dir.path(), id).exists());
    }

    #[test]
    fn a_job_mcr_moved_on_from_supersedes_its_hold() {
        let dir = TempDir::new().unwrap();
        let repo = repo(&dir);
        let retried = held_job(&repo, dir.path(), 1, Duration::hours(24));
        let cancelled = held_job(&repo, dir.path(), 2, Duration::hours(24));
        let marked = held_job(&repo, dir.path(), 3, Duration::hours(24));
        let waiting = held_job(&repo, dir.path(), 4, Duration::hours(24));
        assert!(repo.retry_job(retried, None).unwrap());
        assert_eq!(repo.cancel_job(cancelled).unwrap(), crate::repository::CancelOutcome::Cancelled);
        assert!(repo.mark_completed_manually(marked).unwrap());

        // retry_job superseded its row itself; the other two are for the sweep.
        assert_eq!(repo.get_held(retried).unwrap().unwrap().decision.as_deref(), Some("superseded"));
        let report = sweep(&repo, dir.path(), Utc::now()).unwrap();
        let mut got = report.superseded.clone();
        got.sort();
        assert_eq!(got, vec![cancelled, marked]);
        for id in [retried, cancelled, marked] {
            assert!(!held_dir(dir.path(), id).exists(), "job {id}");
        }
        assert!(held_dir(dir.path(), waiting).exists(), "a job still waiting keeps its file");
        assert_eq!(repo.get_job(waiting).unwrap().unwrap().status, JobStatus::AwaitingApproval);
    }

    #[test]
    fn an_approved_row_survives_while_its_job_is_pending_or_running_and_goes_after() {
        let dir = TempDir::new().unwrap();
        let repo = repo(&dir);
        let id = held_job(&repo, dir.path(), 1, Duration::hours(24));
        let sha = repo.get_held(id).unwrap().unwrap().sha256;
        assert!(repo.approve_held(id, &sha, "mcr").unwrap());
        assert!(sweep(&repo, dir.path(), Utc::now()).unwrap().is_empty());
        assert!(held_dir(dir.path(), id).exists());
        // MCR cancels the job before it is leased.
        assert!(repo.cancel_job(id).is_ok());
        let report = sweep(&repo, dir.path(), Utc::now()).unwrap();
        assert_eq!(report.superseded, vec![id]);
        assert!(!held_dir(dir.path(), id).exists());
    }

    #[test]
    fn a_row_whose_job_is_gone_is_forgotten() {
        let dir = TempDir::new().unwrap();
        let repo = repo(&dir);
        let id = held_job(&repo, dir.path(), 1, Duration::hours(24));
        repo.delete_job(id).unwrap();
        let report = sweep(&repo, dir.path(), Utc::now()).unwrap();
        assert_eq!(report.forgotten, vec![id]);
        assert!(repo.get_held(id).unwrap().is_none());
        assert!(!held_dir(dir.path(), id).exists());
    }

    #[test]
    fn stray_directories_go_but_only_whole_number_names_are_touched() {
        let dir = TempDir::new().unwrap();
        let repo = repo(&dir);
        let id = held_job(&repo, dir.path(), 1, Duration::hours(24));
        let held = dir.path().join("held");
        // No row: stray. Prefix-alikes of a live job and non-numeric names stay.
        for name in ["99", "1x", "x1", "10", "1.bak", "-5"] {
            std::fs::create_dir_all(held.join(name)).unwrap();
            std::fs::write(held.join(name).join("f"), b"x").unwrap();
        }
        // A plain file named like a job is not a directory: untouched.
        std::fs::write(held.join("77"), b"x").unwrap();
        let report = sweep(&repo, dir.path(), Utc::now()).unwrap();
        // "99" and "10" are numeric with no row and no running job.
        assert_eq!(report.dirs_removed, 2);
        for gone in ["99", "10"] {
            assert!(!held.join(gone).exists(), "{gone}");
        }
        for kept in ["1x", "x1", "1.bak", "-5", "77"] {
            assert!(held.join(kept).exists(), "{kept} must be left alone");
        }
        assert!(held_dir(dir.path(), id).exists());
    }

    #[test]
    fn a_running_jobs_directory_without_its_row_yet_is_left_alone() {
        let dir = TempDir::new().unwrap();
        let repo = repo(&dir);
        let id = job(&repo, 1);
        repo.lease_job("w", 180).unwrap().unwrap();
        std::fs::create_dir_all(held_dir(dir.path(), id)).unwrap();
        assert_eq!(sweep(&repo, dir.path(), Utc::now()).unwrap().dirs_removed, 0);
        assert!(held_dir(dir.path(), id).exists());
    }

    /// A retried job holds again: its new file is in the directory before
    /// `hold_output` replaces the old (final) row. The pass must not take it.
    #[test]
    fn a_running_jobs_directory_survives_whatever_its_old_row_says() {
        let dir = TempDir::new().unwrap();
        let repo = repo(&dir);
        let id = held_job(&repo, dir.path(), 1, Duration::hours(24));
        assert!(repo.retry_job(id, None).unwrap()); // old row: superseded
        assert_eq!(repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("superseded"));
        repo.lease_job("w", 180).unwrap().expect("leased again");
        assert!(held_dir(dir.path(), id).exists());
        assert_eq!(sweep(&repo, dir.path(), Utc::now()).unwrap().dirs_removed, 0);
        assert!(held_dir(dir.path(), id).exists(), "the new hold's directory was removed under it");
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_handle_stops_writers_and_deleters_but_not_readers() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("a.bin");
        std::fs::write(&f, b"abc").unwrap();
        let handle = open_locked(&f).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&f).is_err(), "a writer got in");
        assert!(std::fs::remove_file(&f).is_err(), "the file was deleted under the handle");
        assert_eq!(sha256_file(&f).unwrap().len(), 64, "readers must still work");
        drop(handle);
        assert!(std::fs::remove_file(&f).is_ok());
    }

    #[test]
    fn a_final_decision_removes_a_leftover_directory() {
        let dir = TempDir::new().unwrap();
        let repo = repo(&dir);
        let id = held_job(&repo, dir.path(), 1, Duration::hours(24));
        assert!(repo.reject_held(id, "not_usable", "msg", "mcr").unwrap());
        // The route's own removal failed; the sweep retries.
        assert!(held_dir(dir.path(), id).exists());
        assert_eq!(sweep(&repo, dir.path(), Utc::now()).unwrap().dirs_removed, 1);
        assert!(!held_dir(dir.path(), id).exists());
    }
}
