//! The queue side of the second route (P11.6): a job takes it once, and only
//! the worker holding its lease can take it. Real SQLite files; every name and
//! address is invented.

use omni_core::models::{JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const OWNER: &str = "host:1:0";

fn queued(repo: &Repository, n: u32) -> i64 {
    repo.enqueue(&NewJob::new(format!("https://news.example/story-{n}"), format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id()
}

#[test]
fn a_job_takes_its_second_route_once_and_only_under_its_own_lease() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    let id = queued(&repo, 1);

    // Not leased: nobody may take it.
    assert!(!repo.claim_second_route(id, OWNER).unwrap());
    assert_eq!(repo.second_route_at(id).unwrap(), None);

    assert!(repo.lease_job_by_id(id, OWNER, 180).unwrap());
    // Another worker's claim on a job it does not hold writes nothing.
    assert!(!repo.claim_second_route(id, "host:2:0").unwrap());
    assert_eq!(repo.second_route_at(id).unwrap(), None);

    assert!(repo.claim_second_route(id, OWNER).unwrap(), "the lease holder takes it");
    let at = repo.second_route_at(id).unwrap().expect("recorded");
    assert!((chrono::Utc::now() - at).num_seconds().abs() < 60);
    assert!(!repo.claim_second_route(id, OWNER).unwrap(), "only once");

    // Still once after the job ends and is leased again (a restart, a retry).
    assert!(repo.finish(id, OWNER, JobStatus::RequiresReview, Some("TRANSCODE_FAILED"), Some("x"), None).unwrap());
    // MCR's retry puts it back in the queue.
    assert!(repo.retry_job(id, None).unwrap());
    assert!(repo.lease_job_by_id(id, OWNER, 180).unwrap());
    assert!(!repo.claim_second_route(id, OWNER).unwrap(), "a job takes the second route once, ever");
    assert_eq!(repo.second_route_at(id).unwrap(), Some(at));

    // Another job is untouched.
    let other = queued(&repo, 2);
    assert_eq!(repo.second_route_at(other).unwrap(), None);
    assert_eq!(repo.second_route_at(9_999).unwrap(), None, "a missing row has none");
}
