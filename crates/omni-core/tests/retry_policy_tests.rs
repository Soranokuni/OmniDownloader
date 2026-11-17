//! The queue side of the retry policy (P11.3a): `requeue_retry`'s attempts and
//! `max_attempts`, and `retry_from`, which only MCR's fresh starts write.
//! Real SQLite files; every name and address is invented.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use tempfile::TempDir;

use omni_core::models::{JobStatus, NewJob};
use omni_core::repository::{AttemptEnd, Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const OWNER: &str = "host:1:0";

struct Rig {
    _dir: TempDir,
    db: std::path::PathBuf,
    repo: Repository,
}

fn rig() -> Rig {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("omni.db");
    let repo = Repository::new(&db).unwrap();
    Rig { _dir: dir, db, repo }
}

impl Rig {
    fn queue(&self, n: u32) -> i64 {
        self.repo
            .enqueue(&NewJob::new(format!("https://news.example/story-{n}"), format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id()
    }

    /// Lease job `id` as OWNER, its back-off (if any) over.
    fn lease(&self, id: i64) {
        self.sql("UPDATE queue SET not_before = NULL WHERE id = ?1", id);
        assert!(self.repo.lease_job_by_id(id, OWNER, 180).unwrap(), "job {id} not leased");
    }

    fn row(&self, id: i64) -> omni_core::models::Job {
        self.repo.get_job(id).unwrap().unwrap()
    }

    fn sql(&self, sql: &str, id: i64) {
        Connection::open(&self.db).unwrap().execute(sql, params![id]).unwrap();
    }

    /// Leased by OWNER and parked for approval, with a repair attempt whose
    /// verified list is `a.mp4`, `b.mp4` (the first held).
    fn held(&self, n: u32) -> i64 {
        let id = self.queue(n);
        self.lease(id);
        let at = self.repo.start_attempt(id, "hold", Utc::now()).unwrap();
        let list = serde_json::json!([
            { "url": "https://cdn.news.example/v/a.mp4", "origin": "recipe:x" },
            { "url": "https://cdn.news.example/v/b.mp4", "origin": "recipe:x" }
        ]);
        let end = AttemptEnd {
            rung: Some("recipes".into()),
            outcome: Some("held".into()),
            domain: Some("news.example".into()),
            candidates_json: Some(list.to_string()),
            held_url: Some("https://cdn.news.example/v/a.mp4".into()),
            next_index: 0,
            ..Default::default()
        };
        self.repo.finish_attempt(at, &end, Utc::now()).unwrap();
        assert!(self.repo.hold_output(id, OWNER, "temp/held/x/clip.mxf", 10, &sha(), Some(9.0), Utc::now() + Duration::hours(24)).unwrap());
        assert!(self.repo.finish(id, OWNER, JobStatus::AwaitingApproval, None, None, None).unwrap());
        id
    }
}

fn sha() -> String {
    "ab".repeat(32)
}

fn just_now(at: Option<DateTime<Utc>>) -> bool {
    at.is_some_and(|t| (Utc::now() - t).num_seconds().abs() < 30)
}

#[test]
fn an_ordinary_retry_spends_its_attempt_and_shows_the_codes_budget() -> Result<()> {
    let r = rig();
    let id = r.queue(1);
    r.lease(id);
    assert_eq!((r.row(id).attempts, r.row(id).max_attempts), (1, 3), "the column's default stays 3");
    assert!(r.repo.requeue_retry(id, OWNER, Duration::seconds(30), "NETWORK", "reset", 5)?);
    let job = r.row(id);
    assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, 1, 5));
    assert_eq!((job.error_code.as_deref(), job.lease_owner), (Some("NETWORK"), None));
    let wait = job.not_before.expect("a back-off") - Utc::now();
    assert!(wait > Duration::seconds(20) && wait <= Duration::seconds(31), "{wait:?}");
    Ok(())
}

/// A LOW_DISK retry is an ordinary one (review round 1): each lease's attempt
/// stays spent, and the desk shows its budget of four.
#[test]
fn a_low_disk_retry_spends_its_attempt_like_any_other() -> Result<()> {
    let r = rig();
    let id = r.queue(1);
    for n in 1..=3 {
        r.lease(id);
        assert_eq!(r.row(id).attempts, n);
        assert!(r.repo.requeue_retry(id, OWNER, Duration::minutes(10), "LOW_DISK", "full", 4)?);
        let job = r.row(id);
        assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, n, 4), "retry {n}");
    }
    // A video that waited for the watchfolder: the requeue ends the wait, the count stays.
    r.lease(id);
    r.sql("UPDATE queue SET attempts = 2, delivery_wait = 1 WHERE id = ?1", id);
    assert!(r.repo.requeue_retry(id, OWNER, Duration::minutes(10), "LOW_DISK", "full", 4)?);
    let job = r.row(id);
    assert_eq!((job.attempts, job.delivery_wait), (2, false));
    Ok(())
}

#[test]
fn only_the_lease_holder_requeues_and_the_plain_requeue_is_unchanged() -> Result<()> {
    let r = rig();
    let id = r.queue(1);
    r.lease(id);
    assert!(!r.repo.requeue_retry(id, "someone-else", Duration::seconds(30), "NETWORK", "x", 5)?);
    let job = r.row(id);
    assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Running, 1, 3), "written for a lease it does not hold");
    // The panic path's requeue: attempts and max_attempts as they were.
    assert!(r.repo.requeue_after(id, OWNER, Duration::seconds(30), "PIPELINE_FAILED", "internal error (panic)")?);
    let job = r.row(id);
    assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, 1, 3));
    Ok(())
}

#[test]
fn enqueue_and_the_worker_leave_retry_from_empty() -> Result<()> {
    let r = rig();
    let id = r.queue(1);
    assert_eq!(r.row(id).retry_from, None, "a new job counts from created_at");
    r.lease(id);
    assert!(r.repo.requeue_retry(id, OWNER, Duration::seconds(-1), "NETWORK", "x", 5)?);
    r.lease(id);
    assert!(r.repo.finish(id, OWNER, JobStatus::RequiresReview, Some("NETWORK"), Some("x"), None)?);
    assert_eq!(r.row(id).retry_from, None, "only MCR's fresh starts write it");
    Ok(())
}

#[test]
fn mcrs_fresh_starts_reset_the_retry_age() -> Result<()> {
    let r = rig();
    // «Δοκιμή ξανά» on a job in review.
    let retried = r.queue(1);
    r.lease(retried);
    assert!(r.repo.finish(retried, OWNER, JobStatus::RequiresReview, Some("NETWORK"), Some("x"), None)?);
    assert!(r.repo.retry_job(retried, None)?);
    let job = r.row(retried);
    assert!(just_now(job.retry_from), "{:?}", job.retry_from);
    assert_eq!(job.attempts, 0);

    // «Νέα λήψη» on a delivered job.
    let again = r.queue(2);
    r.lease(again);
    assert!(r.repo.finish(again, OWNER, JobStatus::Completed, None, None, Some("watch/2_ANNA_TEST.mxf"))?);
    assert_eq!(r.row(again).retry_from, None);
    assert!(r.repo.redownload_job(again)?);
    assert!(just_now(r.row(again).retry_from));

    // An approval of a held video.
    let approved = r.held(3);
    assert_eq!(r.row(approved).retry_from, None);
    assert!(r.repo.approve_held(approved, &sha(), "desk")?);
    assert!(just_now(r.row(approved).retry_from));

    // «Επόμενη επιλογή».
    let next = r.held(4);
    assert_eq!(r.row(next).retry_from, None);
    assert_eq!(r.repo.next_candidate(next, &sha(), "desk", Utc::now())?, Some(1));
    assert!(just_now(r.row(next).retry_from));
    Ok(())
}

#[test]
fn a_refused_fresh_start_writes_no_retry_from() -> Result<()> {
    let r = rig();
    let running = r.queue(1);
    r.lease(running);
    assert!(!r.repo.retry_job(running, None)?, "a running job cannot be retried");
    assert!(!r.repo.redownload_job(running)?);
    assert!(!r.repo.approve_held(running, &sha(), "desk")?);
    assert_eq!(r.row(running).retry_from, None);
    Ok(())
}
