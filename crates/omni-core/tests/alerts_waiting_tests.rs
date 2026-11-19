//! The jobs the Event Log alert counts as waiting for a person (P11.14): in
//! review, for a manual download or held for approval, not cleared from the
//! desk, each with the moment it started waiting. Real SQLite files; every
//! name is invented.

use chrono::{Duration, Utc};
use omni_core::models::{JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const OWNER: &str = "host:1:0";

fn queued(repo: &Repository, n: u32) -> i64 {
    repo.enqueue(&NewJob::new(format!("https://news.example/story-{n}"), format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id()
}

fn ended_as(repo: &Repository, n: u32, status: JobStatus) -> i64 {
    let id = queued(repo, n);
    assert!(repo.lease_job_by_id(id, OWNER, 180).unwrap());
    assert!(repo.finish(id, OWNER, status, Some("TRANSCODE_FAILED"), Some("x"), None).unwrap());
    id
}

#[test]
fn review_manual_and_held_jobs_wait_for_a_person_and_nothing_else_does() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    let before = Utc::now() - Duration::seconds(5);

    let review = ended_as(&repo, 1, JobStatus::RequiresReview);
    let manual = ended_as(&repo, 2, JobStatus::ManualDownload);
    let _done = ended_as(&repo, 3, JobStatus::Completed);
    let _pending = queued(&repo, 4);
    let held = queued(&repo, 5);
    assert!(repo.lease_job_by_id(held, OWNER, 180).unwrap());
    assert!(repo.hold_output(held, OWNER, "C:/temp/held/5/5_ANNA_TEST.mxf", 10, "ab", Some(4.0), Utc::now() + Duration::hours(24)).unwrap());
    assert!(repo.finish(held, OWNER, JobStatus::AwaitingApproval, None, None, None).unwrap());

    let waiting = repo.jobs_waiting_for_person().unwrap();
    let ids: Vec<i64> = waiting.iter().map(|w| w.id).collect();
    assert_eq!(ids.len(), 3, "{waiting:?}");
    for id in [review, manual, held] {
        assert!(ids.contains(&id), "{id} missing: {waiting:?}");
    }
    for w in &waiting {
        assert!(w.since >= before && w.since <= Utc::now() + Duration::seconds(5), "{w:?}");
    }
    let statuses: Vec<&str> = waiting.iter().map(|w| w.status.as_str()).collect();
    assert!(statuses.contains(&"REQUIRES_REVIEW") && statuses.contains(&"MANUAL_DOWNLOAD") && statuses.contains(&"AWAITING_APPROVAL"));

    // Hidden from the desk's review tab: no longer counted.
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(repo.hide_old_review(0).unwrap() >= 1);
    let left: Vec<i64> = repo.jobs_waiting_for_person().unwrap().iter().map(|w| w.id).collect();
    assert_eq!(left, vec![held], "the held video still waits");
}
