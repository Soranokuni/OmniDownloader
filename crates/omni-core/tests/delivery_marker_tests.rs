//! The delivered marker (plan P1.22): a crash after the rename into the
//! watchfolder must finish the job as delivered, never requeue it into a second
//! copy on air. Real SQLite files and real files on disk.

use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use omni_core::models::{JobStage, JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use tempfile::TempDir;

const SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const OWNER: &str = "newsroom1:100:0";
const HOST: &str = "newsroom1";

struct Fixture {
    dir: TempDir,
    repo: Repository,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        std::fs::create_dir_all(dir.path().join("watchfolder")).unwrap();
        Self { dir, repo }
    }

    fn watchfolder(&self) -> PathBuf {
        self.dir.path().join("watchfolder")
    }

    fn queue(&self, n: u32) -> i64 {
        self.repo
            .enqueue(
                &NewJob::new(&format!("https://example.invalid/v{n}"), &format!("{n}_ANNA_TEST"), "ANNA"),
                DEFAULT_DEDUP_WINDOW_HOURS,
            )
            .unwrap()
            .job_id()
    }

    /// A job leased by OWNER and sitting at DELIVER.
    fn at_deliver(&self, n: u32) -> i64 {
        let id = self.queue(n);
        assert_eq!(self.repo.lease_job(OWNER, 3600).unwrap().expect("leased").id, id);
        assert!(self.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
        id
    }

    /// A held job, approved and leased for its delivery.
    fn approved_at_deliver(&self, n: u32) -> i64 {
        let id = self.queue(n);
        assert_eq!(self.repo.lease_job(OWNER, 3600).unwrap().expect("leased").id, id);
        let held_until = Utc::now() + Duration::hours(24);
        assert!(self.repo.hold_output(id, OWNER, "temp/held/x/clip.mxf", 1000, SHA, Some(12.5), held_until).unwrap());
        assert!(self.repo.finish(id, OWNER, JobStatus::AwaitingApproval, None, None, None).unwrap());
        assert!(self.repo.approve_held(id, SHA, "desk 1").unwrap());
        assert_eq!(self.repo.lease_approved_held_job(OWNER, 3600).unwrap().expect("leased").id, id);
        assert!(self.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
        id
    }

    /// A delivered file of `bytes` bytes under its final name, as a rename leaves it.
    fn delivered_file(&self, name: &str, bytes: usize) -> PathBuf {
        let p = self.watchfolder().join(name);
        std::fs::write(&p, vec![7u8; bytes]).unwrap();
        p
    }

    fn marker(&self, id: i64) -> (Option<String>, Option<i64>) {
        let conn = rusqlite::Connection::open(self.dir.path().join("omni.db")).unwrap();
        conn.query_row("SELECT delivering_to, delivering_bytes FROM queue WHERE id = ?", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    }

    fn events(&self, id: i64) -> Vec<(String, String)> {
        self.repo
            .get_job_events(id, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.level, e.message))
            .collect()
    }

    fn audit_count(&self, like: &str) -> i64 {
        let conn = rusqlite::Connection::open(self.dir.path().join("omni.db")).unwrap();
        conn.query_row("SELECT COUNT(*) FROM audit_logs WHERE message LIKE ?", [like], |r| r.get(0)).unwrap()
    }

    fn files_in_watchfolder(&self) -> usize {
        std::fs::read_dir(self.watchfolder()).unwrap().count()
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn a_crash_after_the_rename_finishes_the_job_as_delivered_and_never_requeues_it() {
    let fx = Fixture::new();
    let id = fx.at_deliver(1);
    let file = fx.delivered_file("1_ANNA_TEST.mxf", 4096);
    // What the pipeline writes right before the rename; then the process dies:
    // no set_delivered_file, no finish.
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 4096).unwrap());

    let requeued = fx.repo.recover_on_startup(HOST).unwrap();

    assert_eq!(requeued, 0, "a delivered job was requeued");
    let job = fx.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.file_path.as_deref(), Some(path_str(&file).as_str()));
    assert_eq!(job.lease_owner, None);
    assert_eq!(fx.marker(id), (None, None), "the marker outlived the job");
    let events = fx.events(id);
    assert!(
        events.iter().any(|(_, m)| m == "Recovered after a restart: already delivered to the watchfolder as 1_ANNA_TEST.mxf; archive skipped"),
        "{events:?}"
    );
    assert_eq!(fx.audit_count("Job % restart after the rename%1_ANNA_TEST.mxf%"), 1);
    assert!(fx.repo.lease_job("newsroom1:100:1", 60).unwrap().is_none(), "the job is leasable again");
    assert_eq!(fx.files_in_watchfolder(), 1, "exactly one file must be in the watchfolder");
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 4096, "the delivered file was touched");
}

#[test]
fn a_crash_between_the_recorded_file_and_the_finish_is_still_a_delivery() {
    let fx = Fixture::new();
    let id = fx.at_deliver(2);
    let file = fx.delivered_file("2_ANNA_TEST.mxf", 2048);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 2048).unwrap());
    // Archive is off, so the stage stays DELIVER: before P1.22 this requeued.
    assert!(fx.repo.set_delivered_file(id, OWNER, &path_str(&file), 12.5).unwrap());

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 0);

    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Completed);
    assert_eq!(fx.files_in_watchfolder(), 1);
}

#[test]
fn a_marker_with_no_file_at_that_path_keeps_the_old_behaviour() {
    let fx = Fixture::new();
    let id = fx.at_deliver(3);
    // The crash came before the rename: only the staging file exists.
    std::fs::write(fx.watchfolder().join(".3_ANNA_TEST.mxf.tmp"), b"partial").unwrap();
    let target = fx.watchfolder().join("3_ANNA_TEST.mxf");
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&target), 4096).unwrap());

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 1);

    let job = fx.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!(job.stage, JobStage::Queued);
    assert_eq!(job.file_path, None);
    assert_eq!(fx.marker(id), (None, None));
    assert!(fx.events(id).iter().any(|(_, m)| m == "Recovered after daemon restart; requeued"));
}

#[test]
fn a_file_of_the_wrong_size_is_not_claimed_and_the_job_is_requeued_with_a_warning() {
    let fx = Fixture::new();
    let id = fx.at_deliver(4);
    // Somebody else's file under that name, a different length.
    let other = fx.delivered_file("4_ANNA_TEST.mxf", 100);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&other), 4096).unwrap());

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 1);

    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Pending);
    let events = fx.events(id);
    let warn = events
        .iter()
        .find(|(l, m)| l == "WARN" && m.contains("100 bytes") && m.contains("4096"))
        .unwrap_or_else(|| panic!("no size warning in {events:?}"));
    assert!(warn.1.contains("4_ANNA_TEST.mxf"));
    assert_eq!(std::fs::metadata(&other).unwrap().len(), 100, "the other file was touched");
}

#[test]
fn an_unreachable_watchfolder_sends_the_job_to_review_instead_of_requeuing() {
    let fx = Fixture::new();
    let id = fx.at_deliver(5);
    // The folder itself cannot be read (an unplugged share): "not found" proves nothing.
    let gone = fx.dir.path().join("share-that-is-down").join("5_ANNA_TEST.mxf");
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&gone), 4096).unwrap());

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 0, "counted as requeued");

    let job = fx.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::RequiresReview);
    assert_eq!(job.lease_owner, None);
    assert_eq!(job.error_code.as_deref(), Some("DELIVERY_FAILED"));
    assert!(job.error_message.unwrap_or_default().contains("5_ANNA_TEST.mxf"));
    assert!(fx.repo.lease_job("newsroom1:100:1", 60).unwrap().is_none(), "a review job was leased");
}

#[test]
fn without_a_marker_a_deliver_stage_job_is_requeued_as_before() {
    let fx = Fixture::new();
    let id = fx.at_deliver(6);
    fx.delivered_file("6_ANNA_TEST.mxf", 10);

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 1);

    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Pending);
}

#[test]
fn the_held_approval_delivery_recovers_from_the_same_crash() {
    let fx = Fixture::new();
    let id = fx.approved_at_deliver(7);
    let file = fx.delivered_file("7_ANNA_TEST.mxf", 3000);
    // Crash after the rename, before record_held_delivery.
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 3000).unwrap());

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 0);

    let job = fx.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.file_path.as_deref(), Some(path_str(&file).as_str()));
    let held = fx.repo.get_held(id).unwrap().unwrap();
    assert_eq!(held.decision.as_deref(), Some("delivered"), "the held row stayed approved and would deliver again");
    assert_eq!(held.delivered_path.as_deref(), Some(path_str(&file).as_str()));
    assert!(fx.repo.lease_job("newsroom1:100:1", 60).unwrap().is_none());
    assert_eq!(fx.files_in_watchfolder(), 1);
}

#[test]
fn the_held_delivery_crashed_after_its_record_is_recovered_too() {
    let fx = Fixture::new();
    let id = fx.approved_at_deliver(8);
    let file = fx.delivered_file("8_ANNA_TEST.mxf", 3000);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 3000).unwrap());
    assert!(fx.repo.record_held_delivery(id, OWNER, &path_str(&file), 12.5).unwrap());

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 0);

    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Completed);
    assert_eq!(fx.repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("delivered"));
}

#[test]
fn a_held_delivery_with_no_file_is_requeued_for_its_own_idempotent_path() {
    let fx = Fixture::new();
    let id = fx.approved_at_deliver(9);
    let target = fx.watchfolder().join("9_ANNA_TEST.mxf");
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&target), 3000).unwrap());

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 1);

    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Pending);
    assert_eq!(fx.repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("approved"));
}

#[test]
fn only_the_lease_holder_can_write_the_marker() {
    let fx = Fixture::new();
    let id = fx.at_deliver(10);
    assert!(!fx.repo.mark_delivering(id, "other-host:1:0", "C:/x/10.mxf", 5).unwrap(), "a stranger wrote the marker");
    assert_eq!(fx.marker(id), (None, None));
    assert!(fx.repo.mark_delivering(id, OWNER, "C:/x/10.mxf", 5).unwrap());
    assert_eq!(fx.marker(id), (Some("C:/x/10.mxf".to_string()), Some(5)));

    // After a finish the lease is gone: refused, and the marker is cleared.
    assert!(fx.repo.finish(id, OWNER, JobStatus::Completed, None, None, Some("C:/x/10.mxf")).unwrap());
    assert_eq!(fx.marker(id), (None, None), "finish left the marker behind");
    assert!(!fx.repo.mark_delivering(id, OWNER, "C:/x/10.mxf", 5).unwrap());
}

#[test]
fn a_normal_finish_of_a_held_delivery_clears_the_marker() {
    let fx = Fixture::new();
    let id = fx.approved_at_deliver(11);
    let file = fx.delivered_file("11_ANNA_TEST.mxf", 50);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 50).unwrap());
    assert!(fx.repo.record_held_delivery(id, OWNER, &path_str(&file), 1.0).unwrap());
    assert!(fx.marker(id).0.is_some(), "the marker must survive until the finish");
    assert!(fx.repo.finish_held_delivery(id, OWNER).unwrap());
    assert_eq!(fx.marker(id), (None, None));
}

#[test]
fn the_reaper_never_finishes_a_job_by_marker_and_a_new_lease_clears_it() {
    let fx = Fixture::new();
    let id = fx.queue(12);
    // A lease that has already run out, with the marker and a matching file.
    assert_eq!(fx.repo.lease_job(OWNER, -5).unwrap().expect("leased").id, id);
    let file = fx.delivered_file("12_ANNA_TEST.mxf", 64);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 64).unwrap());

    assert_eq!(fx.repo.reap_expired_leases().unwrap(), (1, 0));
    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Pending);

    assert_eq!(fx.repo.lease_job("newsroom1:100:2", 60).unwrap().expect("leased again").id, id);
    assert_eq!(fx.marker(id), (None, None), "a stale marker survived a new lease");
}

#[test]
fn a_recorded_delivery_is_delivered_even_when_mcr_already_took_the_file() {
    let fx = Fixture::new();
    let id = fx.at_deliver(13);
    let file = fx.delivered_file("13_ANNA_TEST.mxf", 777);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 777).unwrap());
    assert!(fx.repo.set_delivered_file(id, OWNER, &path_str(&file), 12.5).unwrap());
    // MCR moves the file into Dalet by hand before the daemon comes back.
    std::fs::remove_file(&file).unwrap();

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 0, "a delivered job was requeued");

    let job = fx.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.file_path.as_deref(), Some(path_str(&file).as_str()));
    let events = fx.events(id);
    assert!(
        events.iter().any(|(_, m)| m == "Recovered after a restart: already delivered as 13_ANNA_TEST.mxf; archive skipped"),
        "{events:?}"
    );
    assert_eq!(fx.files_in_watchfolder(), 0);
    assert!(fx.repo.lease_job("newsroom1:100:1", 60).unwrap().is_none());
}

#[test]
fn a_recorded_held_delivery_is_delivered_even_when_the_file_is_gone() {
    let fx = Fixture::new();
    let id = fx.approved_at_deliver(14);
    let file = fx.delivered_file("14_ANNA_TEST.mxf", 900);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 900).unwrap());
    assert!(fx.repo.record_held_delivery(id, OWNER, &path_str(&file), 12.5).unwrap());
    std::fs::remove_file(&file).unwrap();

    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 0);

    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Completed);
    assert_eq!(fx.repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("delivered"));
    assert!(fx.repo.lease_job("newsroom1:100:1", 60).unwrap().is_none());
}

#[test]
fn an_earlier_attempts_recorded_path_never_counts_for_a_later_attempt() {
    let fx = Fixture::new();
    let id = fx.at_deliver(15);
    let file = fx.delivered_file("15_ANNA_TEST.mxf", 500);
    // Attempt 1 delivered and recorded the path, then the job failed and MCR retried it.
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 500).unwrap());
    assert!(fx.repo.set_delivered_file(id, OWNER, &path_str(&file), 1.0).unwrap());
    assert!(fx.repo.finish(id, OWNER, JobStatus::Failed, Some("X"), Some("later failure"), None).unwrap());
    assert!(fx.repo.retry_job(id, None).unwrap());
    // Dalet took that file, so the name is free again.
    std::fs::remove_file(&file).unwrap();

    // Attempt 2: a new lease, then a crash before the marker is even written.
    assert_eq!(fx.repo.lease_job(OWNER, 3600).unwrap().expect("leased").id, id);
    assert!(fx.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 1, "a stale file_path counted as a delivery");
    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Pending);

    // Attempt 3: the marker is written for the very same path, then the crash comes
    // before the rename. The old "recorded" state must not survive the new marker.
    assert_eq!(fx.repo.lease_job(OWNER, 3600).unwrap().expect("leased").id, id);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 500).unwrap());
    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 1, "an earlier attempt's record counted");
    assert_eq!(fx.repo.get_job(id).unwrap().unwrap().status, JobStatus::Pending);
}

#[test]
fn recording_a_different_path_than_the_marker_does_not_count_as_recorded() {
    let fx = Fixture::new();
    let id = fx.at_deliver(16);
    let target = fx.watchfolder().join("16_ANNA_TEST.mxf");
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&target), 100).unwrap());
    assert!(fx.repo.set_delivered_file(id, OWNER, "C:/elsewhere/16.mxf", 1.0).unwrap());
    // No file at the marker's path: the old file check applies, and requeues.
    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 1);
}

#[test]
fn the_held_sweep_removes_the_directory_of_a_job_recovered_as_delivered() {
    let fx = Fixture::new();
    let id = fx.approved_at_deliver(17);
    let temp = fx.dir.path().join("temp");
    let held = omni_core::held::held_dir(&temp, id);
    std::fs::create_dir_all(&held).unwrap();
    std::fs::write(held.join("17_ANNA_TEST.mxf"), b"held copy").unwrap();
    let file = fx.delivered_file("17_ANNA_TEST.mxf", 300);
    assert!(fx.repo.mark_delivering(id, OWNER, &path_str(&file), 300).unwrap());
    assert!(fx.repo.record_held_delivery(id, OWNER, &path_str(&file), 1.0).unwrap());
    assert_eq!(fx.repo.recover_on_startup(HOST).unwrap(), 0);
    assert!(held.exists(), "recovery itself must not delete anything");

    let report = omni_core::held::sweep(&fx.repo, &temp, Utc::now()).unwrap();

    assert!(!held.exists(), "the existing sweep left the held directory of a delivered job");
    assert_eq!(report.dirs_removed, 1);
}
