//! "Late" on the MCR desk (P11.3b): the one rule, its count, its list, and
//! the «Τώρα» action that ends a retry's back-off.

use chrono::{Duration, Utc};
use omni_core::models::{Job, JobStatus, JobsFilter, JobsView, LateAfter, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use omni_core::timestamps;

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

fn pending(repo: &Repository, n: usize) -> i64 {
    repo.enqueue(
        &NewJob::new(format!("https://example.gr/v/{n}"), format!("{n}_PAPADAKI_STORY"), "PAPADAKI"),
        DEFAULT_DEDUP_WINDOW_HOURS,
    )
    .unwrap()
    .job_id()
}

fn sql(d: &tempfile::TempDir, query: &str, params: impl rusqlite::Params) {
    let conn = rusqlite::Connection::open(d.path().join("omni.db")).unwrap();
    conn.execute(query, params).unwrap();
}

/// Make the job `mins` old (its `created_at`), with `priority`.
fn age(d: &tempfile::TempDir, id: i64, mins: i64, priority: i32) {
    sql(
        d,
        "UPDATE queue SET created_at = ?, priority = ? WHERE id = ?",
        rusqlite::params![timestamps::format(Utc::now() - Duration::minutes(mins)), priority, id],
    );
}

fn late_ids(repo: &Repository) -> Vec<i64> {
    let filter = JobsFilter { late_after: LateAfter::default(), ..Default::default() };
    repo.list_jobs_page(JobsView::Late, &filter, 1, 50).unwrap().jobs.iter().map(|j| j.id).collect()
}

fn a_job(status: JobStatus) -> Job {
    let (_d, repo) = repo();
    let id = pending(&repo, 1);
    let mut job = repo.get_job(id).unwrap().unwrap();
    job.status = status;
    job.created_at = Some(Utc::now() - Duration::minutes(30));
    job.retry_from = None;
    job.priority = 5;
    job
}

#[test]
fn the_late_rule_has_a_normal_and_an_urgent_limit() {
    let now = Utc::now();
    let limits = LateAfter::default();
    let mut job = a_job(JobStatus::Pending);

    job.created_at = Some(now - Duration::minutes(9));
    assert!(!job.is_late(now, limits), "9 minutes is not late");
    job.created_at = Some(now - Duration::minutes(10));
    assert!(job.is_late(now, limits), "10 minutes is late");

    job.priority = 5 + omni_core::models::URGENT_PRIORITY_BOOST;
    job.created_at = Some(now - Duration::minutes(4));
    assert!(!job.is_late(now, limits), "an urgent job at 4 minutes is not late");
    job.created_at = Some(now - Duration::minutes(5));
    assert!(job.is_late(now, limits), "an urgent job at 5 minutes is late");

    job.status = JobStatus::Running;
    assert!(job.is_late(now, limits), "a running job can be late");
}

#[test]
fn a_job_that_is_not_waiting_or_working_is_never_late() {
    let now = Utc::now();
    for status in [JobStatus::Completed, JobStatus::RequiresReview, JobStatus::Cancelled, JobStatus::Failed] {
        let mut job = a_job(status);
        job.created_at = Some(now - Duration::minutes(600));
        assert!(!job.is_late(now, LateAfter::default()), "{status:?}");
    }
}

#[test]
fn a_restart_restarts_the_age_and_no_timestamp_is_never_late() {
    let now = Utc::now();
    let mut job = a_job(JobStatus::Pending);
    job.created_at = Some(now - Duration::minutes(60));
    assert!(job.is_late(now, LateAfter::default()));
    job.retry_from = Some(now - Duration::minutes(2));
    assert!(!job.is_late(now, LateAfter::default()), "MCR restarted it two minutes ago");

    job.created_at = None;
    job.retry_from = None;
    assert!(!job.is_late(now, LateAfter::default()), "no timestamp: not late, never 'now'");
}

#[test]
fn the_limits_are_clamped_to_one_to_120_minutes() {
    let now = Utc::now();
    let mut job = a_job(JobStatus::Pending);
    job.created_at = Some(now - Duration::minutes(30));
    assert!(job.is_late(now, LateAfter::new(0, -5)), "0 reads as 1 minute");
    assert!(!job.is_late(now, LateAfter::new(100_000, 100_000)), "huge reads as 120 minutes");
    job.created_at = Some(now - Duration::minutes(121));
    assert!(job.is_late(now, LateAfter::new(100_000, 100_000)));
}

#[test]
fn the_count_and_the_late_list_agree_with_the_rule_and_list_the_oldest_first() {
    let (d, repo) = repo();
    let normal_old = pending(&repo, 1); // 20 min
    let normal_young = pending(&repo, 2); // 9 min: not late
    let urgent_late = pending(&repo, 3); // 6 min, urgent: late
    let urgent_young = pending(&repo, 4); // 4 min, urgent: not late
    let restarted = pending(&repo, 5); // old, but restarted 1 minute ago: not late
    let running = pending(&repo, 6); // 40 min, leased below
    let delivered = pending(&repo, 7); // old but finished: never late
    age(&d, normal_old, 20, 5);
    age(&d, normal_young, 9, 5);
    age(&d, urgent_late, 6, 15);
    age(&d, urgent_young, 4, 15);
    age(&d, restarted, 90, 5);
    sql(
        &d,
        "UPDATE queue SET retry_from = ? WHERE id = ?",
        rusqlite::params![timestamps::format(Utc::now() - Duration::minutes(1)), restarted],
    );
    age(&d, running, 40, 5);
    age(&d, delivered, 300, 5);
    sql(&d, "UPDATE queue SET status = 'RUNNING' WHERE id = ?", rusqlite::params![running]);
    sql(&d, "UPDATE queue SET status = 'COMPLETED' WHERE id = ?", rusqlite::params![delivered]);

    assert_eq!(late_ids(&repo), vec![running, normal_old, urgent_late], "oldest first");
    let counts = repo.job_counts(LateAfter::default()).unwrap();
    assert_eq!(counts.late, 3);
    assert_eq!(repo.queue_summary(LateAfter::default()).unwrap().late, 3);

    // Longer limits move the same rows out of the count and the list.
    let long = LateAfter::new(30, 10);
    assert_eq!(repo.job_counts(long).unwrap().late, 1, "only the 40-minute running job");
    let filter = JobsFilter { late_after: long, ..Default::default() };
    let page = repo.list_jobs_page(JobsView::Late, &filter, 1, 50).unwrap();
    assert_eq!((page.total, page.jobs[0].id), (1, running));
}

/// P11.7: a stream still live that waits for its next check is late whatever
/// its age, in the rule, the count and the list alike; not once it waits no
/// more (no back-off left, or MCR's «Τώρα»), nor once it is running or done.
#[test]
fn a_live_stream_waiting_for_its_next_check_is_late_at_any_age() {
    let now = Utc::now();
    let mut job = a_job(JobStatus::Pending);
    job.created_at = Some(now - Duration::minutes(1));
    job.error_code = Some(omni_core::models::LIVE_STREAM_CODE.to_string());
    job.not_before = Some(now + Duration::minutes(10));
    assert!(job.is_late(now, LateAfter::default()), "a waiting live stream one minute old");
    assert!(job.is_late(now, LateAfter::new(120, 120)), "whatever the limit");
    job.not_before = Some(now - Duration::seconds(1));
    assert!(!job.is_late(now, LateAfter::default()), "its check is due: the age rule again");
    job.not_before = None;
    assert!(!job.is_late(now, LateAfter::default()), "no back-off: the age rule again");
    job.not_before = Some(now + Duration::minutes(10));
    job.error_code = Some("NETWORK".into());
    assert!(!job.is_late(now, LateAfter::default()), "another code's back-off is not");
    job.error_code = Some(omni_core::models::LIVE_STREAM_CODE.to_string());
    for status in [JobStatus::Completed, JobStatus::RequiresReview, JobStatus::Cancelled, JobStatus::Running] {
        job.status = status;
        assert!(!job.is_late(now, LateAfter::default()), "{status:?}");
    }

    let (d, repo) = repo();
    let live = pending(&repo, 1);
    let other = pending(&repo, 2);
    let due = pending(&repo, 3);
    let done = pending(&repo, 4);
    let future = timestamps::format(Utc::now() + Duration::minutes(10));
    let past = timestamps::format(Utc::now() - Duration::minutes(1));
    for (id, code, at) in [(live, "LIVE_STREAM", &future), (other, "NETWORK", &future), (due, "LIVE_STREAM", &past), (done, "LIVE_STREAM", &future)] {
        sql(&d, "UPDATE queue SET error_code = ?, not_before = ? WHERE id = ?", rusqlite::params![code, at, id]);
    }
    sql(&d, "UPDATE queue SET status = 'COMPLETED' WHERE id = ?", rusqlite::params![done]);
    assert_eq!(late_ids(&repo), vec![live]);
    assert_eq!(repo.job_counts(LateAfter::default()).unwrap().late, 1);
    assert_eq!(repo.queue_summary(LateAfter::default()).unwrap().late, 1);
    for id in [live, other, due, done] {
        let job = repo.get_job(id).unwrap().unwrap();
        assert_eq!(job.is_late(Utc::now(), LateAfter::default()), id == live, "job {id}: the rule and the SQL disagree");
    }
    // MCR's «Τώρα»: no wait left, so not late until the age rule says so.
    assert!(repo.run_now(live).unwrap());
    assert!(late_ids(&repo).is_empty());
    assert_eq!(repo.job_counts(LateAfter::default()).unwrap().late, 0);
}

#[test]
fn a_job_with_no_created_at_is_not_counted_late() {
    let (d, repo) = repo();
    let id = pending(&repo, 1);
    sql(&d, "UPDATE queue SET created_at = NULL WHERE id = ?", rusqlite::params![id]);
    assert_eq!(repo.job_counts(LateAfter::default()).unwrap().late, 0);
    assert!(late_ids(&repo).is_empty());
}

#[test]
fn run_now_clears_a_back_off_and_nothing_else() {
    let (d, repo) = repo();
    let future = timestamps::format(Utc::now() + Duration::minutes(5));
    let set_back_off = |id: i64| sql(&d, "UPDATE queue SET not_before = ? WHERE id = ?", rusqlite::params![future, id]);

    // PENDING with a back-off: cleared, and the job can be leased at once.
    let waiting = pending(&repo, 1);
    set_back_off(waiting);
    assert!(repo.lease_job("host:1:0", 120).unwrap().is_none(), "in back-off nothing is leased");
    assert!(repo.run_now(waiting).unwrap());
    assert!(repo.get_job(waiting).unwrap().unwrap().not_before.is_none());
    assert!(!repo.run_now(waiting).unwrap(), "twice: nothing left to clear");
    assert_eq!(repo.lease_job("host:1:0", 120).unwrap().unwrap().id, waiting);

    // RUNNING (the lease above): refused, and its not_before is untouched.
    set_back_off(waiting);
    assert!(!repo.run_now(waiting).unwrap());
    assert!(repo.get_job(waiting).unwrap().unwrap().not_before.is_some());

    // PENDING without a back-off.
    let plain = pending(&repo, 2);
    assert!(!repo.run_now(plain).unwrap());

    // Waiting for the watchfolder: the share decides, not the click.
    let parked = pending(&repo, 3);
    set_back_off(parked);
    sql(&d, "UPDATE queue SET delivery_wait = 1 WHERE id = ?", rusqlite::params![parked]);
    assert!(!repo.run_now(parked).unwrap());
    assert!(repo.get_job(parked).unwrap().unwrap().not_before.is_some());

    assert!(!repo.run_now(9999).unwrap(), "no such job");
}
