//! A finished video that waits for an unreachable watchfolder (P11.2a): the
//! queue's side. The wait is leased only while the share answers, straight to
//! DELIVER, without spending attempts, and every write is the lease holder's.
//! Real SQLite files and real files on disk.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use omni_core::models::{JobStage, JobStatus, NewJob};
use omni_core::repository::{CancelOutcome, Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use tempfile::TempDir;

const OWNER: &str = "newsroom1:100:1";
const OTHER: &str = "newsroom1:100:2";
const HOST: &str = "newsroom1";
const SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

struct Fixture {
    dir: TempDir,
    repo: Repository,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        Self { dir, repo }
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

    fn ready_path(&self, id: i64) -> String {
        self.dir.path().join("temp").join("jobs").join(id.to_string()).join(format!("{id}_output.mxf")).to_string_lossy().into_owned()
    }

    fn marker_path(&self, n: u32) -> String {
        self.dir.path().join("watch").join(format!("{n}_ANNA_TEST.mxf")).to_string_lossy().into_owned()
    }

    /// Job `n` leased by OWNER, at DELIVER, its rename marked and then parked:
    /// what a run that found the share down leaves before its requeue.
    fn parked_running(&self, n: u32) -> i64 {
        let id = self.queue(n);
        assert_eq!(self.repo.lease_job(OWNER, 3600).unwrap().expect("leased").id, id);
        assert!(self.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
        assert!(self.repo.mark_delivering(id, OWNER, &self.marker_path(n), 4096).unwrap());
        assert!(self.repo.park_for_delivery(id, OWNER, &self.ready_path(id), 4096, SHA, 12.5).unwrap());
        id
    }

    /// Job `n` parked and back in the queue, ready to be leased now.
    fn parked_pending(&self, n: u32) -> i64 {
        let id = self.parked_running(n);
        assert!(self.repo.requeue_waiting_delivery(id, OWNER, Duration::zero(), "waits").unwrap());
        id
    }

    fn marker(&self, id: i64) -> (Option<String>, Option<i64>) {
        let conn = rusqlite::Connection::open(self.dir.path().join("omni.db")).unwrap();
        conn.query_row("SELECT delivering_to, delivering_bytes FROM queue WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
    }

    fn set_marker_recorded(&self, id: i64) {
        let conn = rusqlite::Connection::open(self.dir.path().join("omni.db")).unwrap();
        conn.execute("UPDATE queue SET delivering_recorded = 1 WHERE id = ?", [id]).unwrap();
    }

    fn attempt_outcome(&self, id: i64) -> Option<String> {
        self.repo.latest_attempt(id).unwrap().and_then(|a| a.outcome)
    }

    fn events(&self, id: i64) -> Vec<String> {
        self.repo.get_job_events(id, 200).unwrap().into_iter().map(|e| e.message).collect()
    }
}

/// Test 3: the share is down, the waiting job is not taken, an ordinary one is;
/// the share is back, the waiting job is leased at DELIVER with its attempts,
/// marker and file as they were. The no-download-slot variant takes it too.
#[test]
fn a_waiting_job_is_leased_only_while_the_share_answers_and_straight_to_deliver() {
    let f = Fixture::new();
    let parked = f.parked_pending(1);
    let ordinary = f.queue(2);
    let before = f.repo.get_job(parked).unwrap().unwrap();
    assert_eq!(before.status, JobStatus::Pending);
    assert!(before.delivery_wait);

    // Share down: the older, waiting job is skipped; the ordinary one is taken.
    assert_eq!(f.repo.lease_job_when(OTHER, 3600, false).unwrap().map(|j| j.id), Some(ordinary));
    assert!(f.repo.lease_job_when(OTHER, 3600, false).unwrap().is_none(), "the waiting job was leased while the share is down");
    assert!(f.repo.lease_approved_held_job_when(OTHER, 3600, false).unwrap().is_none());

    // Share back: leased straight to DELIVER, no attempt spent, everything kept.
    let job = f.repo.lease_job_when(OTHER, 3600, true).unwrap().expect("the waiting job");
    assert_eq!(job.id, parked);
    assert_eq!(job.status, JobStatus::Running);
    assert_eq!(job.stage, JobStage::Deliver);
    assert_eq!(job.attempts, before.attempts, "a delivery that waited spends no attempt");
    assert!(job.delivery_wait);
    assert_eq!(job.ready_path.as_deref(), Some(f.ready_path(parked).as_str()));
    assert_eq!((job.ready_bytes, job.ready_sha256.as_deref()), (Some(4096), Some(SHA)));
    assert_eq!(job.duration_secs, 12.5);
    assert_eq!(f.marker(parked), (Some(f.marker_path(1)), Some(4096)), "the marker of the attempted rename is kept");
}

#[test]
fn the_no_download_slot_lease_takes_a_waiting_job_but_never_an_ordinary_one() {
    let f = Fixture::new();
    let parked = f.parked_pending(1);
    let ordinary = f.queue(2);
    assert_eq!(f.repo.lease_approved_held_job_when(OTHER, 3600, true).unwrap().map(|j| j.id), Some(parked));
    let job = f.repo.get_job(parked).unwrap().unwrap();
    assert_eq!((job.stage, job.attempts), (JobStage::Deliver, 0));
    assert!(f.repo.lease_approved_held_job(OTHER, 3600).unwrap().is_none(), "an ordinary job needs a download slot");
    assert_eq!(f.repo.get_job(ordinary).unwrap().unwrap().status, JobStatus::Pending);
}

/// The lease the worker loop uses on a red disk check: waiting deliveries
/// only, and only while the share answers.
#[test]
fn the_waiting_only_lease_never_takes_an_ordinary_job() {
    let f = Fixture::new();
    let ordinary = f.queue(1);
    let parked = {
        let id = f.queue(2);
        assert_eq!(f.repo.lease_job_by_id(id, OWNER, 3600).unwrap(), true);
        assert!(f.repo.park_for_delivery(id, OWNER, &f.ready_path(id), 4096, SHA, 1.0).unwrap());
        assert!(f.repo.requeue_waiting_delivery(id, OWNER, Duration::zero(), "waits").unwrap());
        id
    };
    assert!(f.repo.lease_waiting_delivery_when(OTHER, 3600, false).unwrap().is_none(), "the share is down");
    assert_eq!(f.repo.lease_waiting_delivery_when(OTHER, 3600, true).unwrap().map(|j| j.id), Some(parked));
    assert!(f.repo.lease_waiting_delivery_when(OTHER, 3600, true).unwrap().is_none(), "the older ordinary job was taken");
    assert_eq!(f.repo.get_job(ordinary).unwrap().unwrap().status, JobStatus::Pending);
}

#[test]
fn waiting_jobs_keep_their_place_in_the_queue() {
    let f = Fixture::new();
    let parked = f.parked_pending(1);
    let newer = f.queue(2);
    assert_eq!(f.repo.lease_job_when(OTHER, 3600, true).unwrap().map(|j| j.id), Some(parked), "older first");
    assert_eq!(f.repo.lease_job_when(OTHER, 3600, true).unwrap().map(|j| j.id), Some(newer));
}

/// Parking gives back the attempt the lease spent once; repeating the wait
/// any number of times never moves it, and never reaches review.
#[test]
fn waiting_again_and_again_never_spends_an_attempt() {
    let f = Fixture::new();
    let id = f.parked_pending(1);
    assert_eq!(f.repo.get_job(id).unwrap().unwrap().attempts, 0, "back to what it was before the run");
    for _ in 0..5 {
        let job = f.repo.lease_job_when(OWNER, 3600, true).unwrap().expect("leased");
        assert_eq!(job.attempts, 0);
        assert!(f.repo.park_for_delivery(id, OWNER, &f.ready_path(id), 4096, SHA, 12.5).unwrap());
        assert!(f.repo.requeue_waiting_delivery(id, OWNER, Duration::zero(), "waits").unwrap());
        let job = f.repo.get_job(id).unwrap().unwrap();
        assert_eq!((job.status, job.attempts, job.delivery_wait), (JobStatus::Pending, 0, true));
        assert_eq!(job.error_code.as_deref(), Some("DELIVERY_FAILED"));
    }
}

/// Test 10 and risk (b): every write is guarded by the lease. A worker that is
/// not the lease holder parks nothing and requeues nothing; and a job parked
/// but not yet requeued is RUNNING, so no other worker can lease it.
#[test]
fn only_the_lease_holder_parks_or_requeues_and_a_parked_running_job_is_not_leasable() {
    let f = Fixture::new();
    let id = f.queue(1);
    assert_eq!(f.repo.lease_job(OWNER, 3600).unwrap().unwrap().id, id);
    assert!(!f.repo.park_for_delivery(id, OTHER, &f.ready_path(id), 4096, SHA, 1.0).unwrap());
    assert!(!f.repo.get_job(id).unwrap().unwrap().delivery_wait);
    assert!(f.repo.park_for_delivery(id, OWNER, &f.ready_path(id), 4096, SHA, 1.0).unwrap());
    assert!(f.repo.lease_job_when(OTHER, 3600, true).unwrap().is_none(), "leased while its worker still holds it");
    assert!(!f.repo.requeue_waiting_delivery(id, OTHER, Duration::zero(), "x").unwrap());
    assert_eq!(f.repo.get_job(id).unwrap().unwrap().status, JobStatus::Running);
    assert!(!f.repo.requeue_for_rerun(id, OTHER, "x").unwrap());

    // A job that does not wait is never requeued as waiting.
    let plain = f.queue(2);
    assert_eq!(f.repo.lease_job(OTHER, 3600).unwrap().unwrap().id, plain);
    assert!(!f.repo.requeue_waiting_delivery(plain, OTHER, Duration::zero(), "x").unwrap());
    assert_eq!(f.repo.get_job(plain).unwrap().unwrap().status, JobStatus::Running);
}

/// Risk (g): an ordinary retry still spends its attempt, and a job that waited
/// and then failed for another reason stops waiting, so its next lease spends
/// one too: nothing but the wait for the share escapes the attempt count.
#[test]
fn an_ordinary_retry_spends_attempts_and_ends_a_wait() {
    let f = Fixture::new();
    let id = f.queue(1);
    for n in 1..=3 {
        let job = f.repo.lease_job(OWNER, 3600).unwrap().expect("leased");
        assert_eq!(job.attempts, n);
        assert!(f.repo.requeue_after(id, OWNER, Duration::zero(), "NETWORK", "reset").unwrap());
        assert_eq!(f.repo.get_job(id).unwrap().unwrap().attempts, n);
    }
    assert_eq!(f.repo.lease_job(OWNER, 3600).unwrap().unwrap().attempts, 4, "never given back");
    assert!(f.repo.finish(id, OWNER, JobStatus::RequiresReview, Some("NETWORK"), Some("x"), None).unwrap());

    let parked = f.parked_pending(2);
    let job = f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    assert_eq!((job.id, job.attempts), (parked, 0));
    assert!(f.repo.requeue_after(parked, OWNER, Duration::zero(), "PIPELINE_FAILED", "x").unwrap());
    let job = f.repo.get_job(parked).unwrap().unwrap();
    assert!(!job.delivery_wait);
    assert_eq!((job.ready_path, job.ready_bytes, job.ready_sha256), (None, None, None));
    let job = f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    assert_eq!((job.stage, job.attempts), (JobStage::Extract, 1), "a full run, which spends an attempt");
}

/// A finish of any kind ends the wait; the delivered file recorded ends it too.
#[test]
fn recording_the_delivery_or_finishing_ends_the_wait() {
    let f = Fixture::new();
    let id = f.parked_pending(1);
    f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    assert!(f.repo.set_delivered_file(id, OWNER, &f.marker_path(1), 12.5).unwrap());
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert!(!job.delivery_wait);
    assert_eq!((job.ready_path, job.ready_bytes, job.ready_sha256), (None, None, None));
    assert_eq!(job.status, JobStatus::Running, "the worker finishes it, not the record");

    let id = f.parked_pending(2);
    f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    assert!(f.repo.finish(id, OWNER, JobStatus::RequiresReview, Some("PIPELINE_FAILED"), Some("x"), None).unwrap());
    assert!(!f.repo.get_job(id).unwrap().unwrap().delivery_wait);
    assert!(f.repo.parked_job_ids().unwrap().is_empty());
}

#[test]
fn a_rerun_clears_the_wait_the_file_and_the_marker() {
    let f = Fixture::new();
    let id = f.parked_pending(1);
    f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    assert!(f.repo.requeue_for_rerun(id, OWNER, "The parked video is missing or changed; running the job again").unwrap());
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert_eq!((job.status, job.stage, job.delivery_wait), (JobStatus::Pending, JobStage::Queued, false));
    assert_eq!((job.ready_path, job.ready_bytes, job.ready_sha256), (None, None, None));
    assert_eq!(f.marker(id), (None, None));
    let job = f.repo.lease_job_when(OWNER, 3600, false).unwrap().expect("an ordinary job now, even with the share down");
    assert_eq!((job.stage, job.attempts), (JobStage::Extract, 1));
}

/// Operator actions on a waiting job: cancel and retry end the wait.
#[test]
fn cancel_and_retry_end_the_wait() {
    let f = Fixture::new();
    let id = f.parked_pending(1);
    assert_eq!(f.repo.cancel_job(id).unwrap(), CancelOutcome::Cancelled);
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Cancelled);
    assert!(!job.delivery_wait && job.ready_path.is_none());
    assert!(f.repo.parked_job_ids().unwrap().is_empty(), "the start-up sweep may take its workspace");

    let id = f.parked_pending(2);
    assert!(f.repo.retry_job(id, None).unwrap());
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Pending);
    assert!(!job.delivery_wait && job.ready_path.is_none());
    let job = f.repo.lease_job_when(OWNER, 3600, false).unwrap().expect("a full run");
    assert_eq!((job.id, job.stage), (id, JobStage::Extract));
    assert_eq!(f.marker(id), (None, None), "the full run's lease drops the old marker");

    // A waiting job that is being delivered cannot be cancelled: the copy is under way.
    let id = f.parked_pending(3);
    f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    assert_eq!(f.repo.cancel_job(id).unwrap(), CancelOutcome::TooLate);
}

/// Finding 1: a waiting job's file is made and its clip named, so MCR's
/// rename answers TOO_LATE (`None`, as the route reads it), queued or leased.
#[test]
fn a_waiting_job_cannot_be_renamed() {
    let f = Fixture::new();
    let id = f.parked_pending(1);
    assert!(f.repo.rename_job_keyword(id, "OTHERWORD").unwrap().is_none(), "a waiting job was renamed");
    assert_eq!(f.repo.get_job(id).unwrap().unwrap().slug, "1_ANNA_TEST");
    f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    assert!(f.repo.rename_job_keyword(id, "OTHERWORD").unwrap().is_none());
    // Control: an ordinary waiting job still renames.
    let plain = f.queue(2);
    assert!(f.repo.rename_job_keyword(plain, "OTHERWORD").unwrap().is_some());
}

/// Finding 2: review of a waiting delivery keeps the marker, the wait and
/// the file; «Το έβαλα στο Dalet» then works and ends the wait, and so does a retry.
#[test]
fn a_waiting_delivery_sent_to_review_keeps_everything_until_mcr_decides() {
    let f = Fixture::new();
    let id = f.parked_pending(1);
    f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    let msg = "The delivery of 1_ANNA_TEST.mxf was interrupted and the file is not in the watchfolder now. Check Dalet for 1_ANNA_TEST.mxf before retrying";
    assert!(!f.repo.review_waiting_delivery(id, OTHER, msg).unwrap(), "not the lease holder");
    assert!(f.repo.review_waiting_delivery(id, OWNER, msg).unwrap());
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert_eq!((job.status, job.error_code.as_deref(), job.error_message.as_deref()), (JobStatus::RequiresReview, Some("DELIVERY_FAILED"), Some(msg)));
    assert_eq!(job.lease_owner, None);
    assert!(job.delivery_wait && job.ready_path.is_some(), "the waiting file is kept");
    assert_eq!(f.marker(id), (Some(f.marker_path(1)), Some(4096)), "the marker is kept");
    assert!(f.events(id).iter().any(|e| e == msg));
    assert!(f.repo.get_recent_audit_logs(50).unwrap().iter().any(|a| a.message == format!("Job {id}: {msg}")));
    assert!(f.repo.lease_job_when(OTHER, 3600, true).unwrap().is_none(), "a job in review is not delivered");
    assert!(f.repo.parked_job_ids().unwrap().is_empty());

    assert!(f.repo.mark_completed_manually(id).unwrap());
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::CompletedManual);
    assert!(!job.delivery_wait);
    assert_eq!((job.ready_path, job.ready_bytes, job.ready_sha256), (None, None, None));

    let id = f.parked_pending(2);
    f.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    assert!(f.repo.review_waiting_delivery(id, OWNER, msg).unwrap());
    assert!(f.repo.retry_job(id, None).unwrap());
    let job = f.repo.lease_job_when(OWNER, 3600, true).unwrap().expect("a full run");
    assert_eq!((job.id, job.stage, job.delivery_wait), (id, JobStage::Extract, false));
}

/// Finding 3: the automatic delivery that waited is promoted once.
#[test]
fn an_automatic_delivery_that_waited_is_promoted_once() {
    let f = Fixture::new();
    let id = f.parked_pending(1);
    let at = f.repo.start_attempt(id, "hold", Utc::now()).unwrap();
    f.repo.mark_attempt_auto_delivering(at, 7, "abc123", "news.example.invalid", "https://cdn.example.invalid/a.mp4").unwrap();
    assert!(f.repo.promote_attempt_auto_delivered(at).unwrap());
    assert!(!f.repo.promote_attempt_auto_delivered(at).unwrap(), "counted twice");
    assert_eq!(f.attempt_outcome(id).as_deref(), Some("auto_delivered"));
}

#[test]
fn the_parked_ids_are_the_waiting_queued_and_running_jobs() {
    let f = Fixture::new();
    let running = f.parked_running(1);
    let pending = f.parked_pending(2);
    f.queue(3);
    let mut ids = f.repo.parked_job_ids().unwrap();
    ids.sort();
    assert_eq!(ids, vec![running, pending]);
}

/// Test 8 / risk (e): the daemon died between parking and the requeue, with a
/// share that cannot be read. Before P11.2a this went to review; it waits now,
/// marker and file kept, no attempt spent.
#[test]
fn a_waiting_job_found_running_at_start_up_waits_again_whatever_the_share_says() {
    let f = Fixture::new();
    let id = f.queue(1);
    assert_eq!(f.repo.lease_job(OWNER, 3600).unwrap().unwrap().id, id);
    assert!(f.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
    // A marker whose folder does not exist: the share "cannot be read".
    let unreadable = f.dir.path().join("no-share").join("1_ANNA_TEST.mxf").to_string_lossy().into_owned();
    assert!(f.repo.mark_delivering(id, OWNER, &unreadable, 4096).unwrap());
    assert!(f.repo.park_for_delivery(id, OWNER, &f.ready_path(id), 4096, SHA, 12.5).unwrap());

    assert_eq!(f.repo.recover_on_startup(HOST).unwrap(), 1);
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Pending, "a waiting job is not sent to review: {:?}", job.error_message);
    assert_eq!(job.stage, JobStage::Queued);
    assert!(job.delivery_wait);
    assert_eq!(job.attempts, 0);
    assert_eq!(job.lease_owner, None);
    assert_eq!((job.ready_bytes, job.ready_sha256.as_deref()), (Some(4096), Some(SHA)));
    assert_eq!(f.marker(id), (Some(unreadable), Some(4096)), "the marker is what the delivery after the wait asks");
    assert!(f.events(id).iter().any(|e| e == "Recovered after daemon restart; the finished video still waits for the watchfolder"));
    assert_eq!(f.repo.lease_job_when(OTHER, 3600, true).unwrap().map(|j| (j.id, j.stage)), Some((id, JobStage::Deliver)));
}

/// The rename landed although the run parked: start-up finishes it as
/// delivered, as for any job, and the wait ends.
#[test]
fn a_waiting_job_whose_file_is_in_the_watchfolder_is_finished_at_start_up() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.dir.path().join("watch")).unwrap();
    let id = f.parked_running(1);
    std::fs::write(f.marker_path(1), vec![7u8; 4096]).unwrap();
    assert_eq!(f.repo.recover_on_startup(HOST).unwrap(), 0);
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.file_path.as_deref(), Some(f.marker_path(1).as_str()));
    assert!(!job.delivery_wait && job.ready_path.is_none());
}

/// Every other start-up case is as before: a RUNNING job that does not wait
/// and whose marker cannot be read still goes to review.
#[test]
fn a_job_that_does_not_wait_is_recovered_exactly_as_before() {
    let f = Fixture::new();
    let id = f.queue(1);
    f.repo.lease_job(OWNER, 3600).unwrap().unwrap();
    assert!(f.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
    let unreadable = f.dir.path().join("no-share").join("1_ANNA_TEST.mxf").to_string_lossy().into_owned();
    assert!(f.repo.mark_delivering(id, OWNER, &unreadable, 4096).unwrap());
    assert_eq!(f.repo.recover_on_startup(HOST).unwrap(), 0);
    let job = f.repo.get_job(id).unwrap().unwrap();
    assert_eq!((job.status, job.error_code.as_deref()), (JobStatus::RequiresReview, Some("DELIVERY_FAILED")));
}

/// Risk (c): a trusted recipe's video that waits keeps its automatic-delivery
/// marker through the requeue, the lease and a restart, so the job's card
/// says «delivered without approval» once it is COMPLETED.
#[test]
fn a_trusted_recipes_waiting_video_keeps_its_automatic_delivery_marker() {
    let f = Fixture::new();
    let id = f.parked_running(1);
    let at = f.repo.start_attempt(id, "hold", Utc::now()).unwrap();
    f.repo.mark_attempt_auto_delivering(at, 7, "abc123", "news.example.invalid", "https://cdn.example.invalid/a.mp4").unwrap();
    assert!(f.repo.requeue_waiting_delivery(id, OWNER, Duration::zero(), "waits").unwrap());
    assert_eq!(f.attempt_outcome(id).as_deref(), Some("auto_delivering"));

    // Leased for the delivery after the wait: not «interrupted».
    f.repo.lease_job_when(OTHER, 3600, true).unwrap().unwrap();
    assert_eq!(f.attempt_outcome(id).as_deref(), Some("auto_delivering"));
    // A restart while it is delivered: still waiting, still marked, no warning to check Dalet.
    assert_eq!(f.repo.recover_on_startup(HOST).unwrap(), 1);
    assert_eq!(f.attempt_outcome(id).as_deref(), Some("auto_delivering"));
    assert!(!f.events(id).iter().any(|e| e.contains("ελέγξτε αν το βίντεο είναι ήδη στο Dalet")));

    // Delivered and finished: the marker counts as the automatic delivery.
    f.repo.lease_job_when(OTHER, 3600, true).unwrap().unwrap();
    assert!(f.repo.set_delivered_file(id, OTHER, &f.marker_path(1), 12.5).unwrap());
    assert!(f.repo.finish(id, OTHER, JobStatus::Completed, None, None, Some(&f.marker_path(1))).unwrap());
    let attempt = f.repo.latest_attempt(id).unwrap().unwrap();
    assert_eq!(attempt.outcome.as_deref(), Some("auto_delivering"));
    assert_eq!(f.repo.get_job(id).unwrap().unwrap().status, JobStatus::Completed);

    // Control: an ordinary lease (the wait ended) still interrupts a leftover marker.
    let plain = f.queue(2);
    f.repo.lease_job(OWNER, 3600).unwrap().unwrap();
    let at = f.repo.start_attempt(plain, "hold", Utc::now()).unwrap();
    f.repo.mark_attempt_auto_delivering(at, 7, "abc123", "news.example.invalid", "https://cdn.example.invalid/b.mp4").unwrap();
    assert!(f.repo.requeue_after(plain, OWNER, Duration::zero(), "NETWORK", "x").unwrap());
    f.repo.lease_job(OWNER, 3600).unwrap().unwrap();
    assert_eq!(f.attempt_outcome(plain).as_deref(), Some("interrupted"));
}

/// The marker, asked through the repository (P11.2a): recorded, there, missing, unknown.
#[test]
fn the_marker_check_says_what_the_watchfolder_holds() {
    use omni_core::repository::MarkerCheck;
    let f = Fixture::new();
    std::fs::create_dir_all(f.dir.path().join("watch")).unwrap();
    let id = f.parked_running(1);
    assert_eq!(f.repo.check_delivery_marker(id).unwrap(), Some(MarkerCheck::Missing));
    std::fs::write(f.marker_path(1), vec![1u8; 4096]).unwrap();
    assert_eq!(f.repo.check_delivery_marker(id).unwrap(), Some(MarkerCheck::Delivered { path: f.marker_path(1) }));
    std::fs::write(f.marker_path(1), vec![1u8; 10]).unwrap();
    assert!(matches!(f.repo.check_delivery_marker(id).unwrap(), Some(MarkerCheck::WrongSize { found: 10, expected: 4096, .. })));
    f.set_marker_recorded(id);
    assert_eq!(f.repo.check_delivery_marker(id).unwrap(), Some(MarkerCheck::Recorded { path: f.marker_path(1) }));

    let other = f.queue(2);
    assert_eq!(f.repo.check_delivery_marker(other).unwrap(), None);
    let gone: PathBuf = f.dir.path().join("no-share").join("x.mxf");
    f.repo.lease_job(OWNER, 3600).unwrap().unwrap();
    assert!(f.repo.mark_delivering(other, OWNER, &gone.to_string_lossy(), 1).unwrap());
    assert!(matches!(f.repo.check_delivery_marker(other).unwrap(), Some(MarkerCheck::Unknown { .. })));
}
