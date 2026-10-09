//! The MCR desk lists (plan P7.1): paging, clearing finished jobs from the
//! live queue without losing them, and downloading a delivered job again.

use omni_core::models::{JobStatus, JobsFilter, JobsView, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

/// Queue a job and run it to `status` the way a worker would.
fn job(repo: &Repository, n: usize, status: JobStatus) -> i64 {
    let id = repo
        .enqueue(
            &NewJob::new(format!("https://example.gr/v/{n}"), format!("{n}_PAPADAKI_STORY"), "PAPADAKI"),
            DEFAULT_DEDUP_WINDOW_HOURS,
        )
        .unwrap()
        .job_id();
    if status != JobStatus::Pending {
        let leased = repo.lease_job("host:1:0", 120).unwrap().expect("lease");
        assert_eq!(leased.id, id);
        let file = (status == JobStatus::Completed).then(|| format!("W:/watch/{n}_PAPADAKI_STORY.mxf"));
        repo.finish(id, "host:1:0", status, None, None, file.as_deref()).unwrap();
    }
    id
}

fn ids(repo: &Repository, view: JobsView, page: i64, per_page: i64) -> (Vec<i64>, i64) {
    let p = repo.list_jobs_page(view, &JobsFilter::default(), page, per_page).unwrap();
    (p.jobs.iter().map(|j| j.id).collect(), p.total)
}

#[test]
fn each_tab_holds_its_jobs_in_pages() {
    let (_d, repo) = repo();
    let done: Vec<i64> = (0..12).map(|n| job(&repo, n, JobStatus::Completed)).collect();
    let review = job(&repo, 100, JobStatus::RequiresReview);
    let waiting = job(&repo, 101, JobStatus::Pending);

    // Live: what is waiting first, then delivered, newest first.
    let (live, total) = ids(&repo, JobsView::Live, 1, 5);
    assert_eq!(total, 13);
    assert_eq!(live[0], waiting);
    assert_eq!(&live[1..], &[done[11], done[10], done[9], done[8]]);
    let (page3, _) = ids(&repo, JobsView::Live, 3, 5);
    assert_eq!(page3, vec![done[2], done[1], done[0]]);

    assert_eq!(ids(&repo, JobsView::Review, 1, 20), (vec![review], 1));
    assert_eq!(ids(&repo, JobsView::Completed, 1, 20).1, 12);

    let c = repo.job_counts().unwrap();
    assert_eq!((c.active, c.finished, c.review, c.completed), (1, 12, 1, 12));
}

#[test]
fn clearing_finished_jobs_empties_the_live_queue_but_keeps_them_under_completed() {
    let (_d, repo) = repo();
    let done = job(&repo, 1, JobStatus::Completed);
    let waiting = job(&repo, 2, JobStatus::Pending);

    assert_eq!(repo.clear_finished_jobs().unwrap(), 1);
    assert_eq!(ids(&repo, JobsView::Live, 1, 20), (vec![waiting], 1));
    assert_eq!(ids(&repo, JobsView::Completed, 1, 20), (vec![done], 1));
    assert!(repo.get_job(done).unwrap().is_some(), "nothing is deleted");
    assert_eq!(repo.clear_finished_jobs().unwrap(), 0);
}

#[test]
fn a_delivered_job_can_be_downloaded_again_and_comes_back_to_the_live_queue() {
    let (_d, repo) = repo();
    let done = job(&repo, 1, JobStatus::Completed);
    repo.clear_finished_jobs().unwrap();

    assert!(repo.redownload_job(done).unwrap());
    let j = repo.get_job(done).unwrap().unwrap();
    assert_eq!(j.status, JobStatus::Pending);
    assert_eq!(j.attempts, 0);
    assert!(j.file_path.is_none() && j.completed_at.is_none() && j.error_code.is_none());
    assert_eq!(ids(&repo, JobsView::Live, 1, 20), (vec![done], 1));
    // A worker picks it up again.
    assert_eq!(repo.lease_job("host:1:0", 120).unwrap().map(|j| j.id), Some(done));

    // Only a delivered job: a waiting or running one is refused.
    let waiting = job(&repo, 2, JobStatus::Pending);
    assert!(!repo.redownload_job(waiting).unwrap());
}

#[test]
fn search_and_filters_narrow_a_list() {
    let (_d, repo) = repo();
    job(&repo, 1, JobStatus::Completed);
    let other = repo
        .enqueue(&NewJob::new("https://example.gr/seismos", "1_NIKOLAOU_SEISMOS", "NIKOLAOU"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    let leased = repo.lease_job("host:1:0", 120).unwrap().unwrap();
    assert_eq!(leased.id, other);
    repo.finish(other, "host:1:0", JobStatus::Completed, None, None, None).unwrap();

    let find = |f: JobsFilter| {
        repo.list_jobs_page(JobsView::Completed, &f, 1, 20).unwrap().jobs.into_iter().map(|j| j.id).collect::<Vec<_>>()
    };
    assert_eq!(find(JobsFilter { search: "seism".into(), ..Default::default() }), vec![other]);
    assert_eq!(find(JobsFilter { journalist: "nikolaou".into(), ..Default::default() }), vec![other]);
    // LIKE wildcards in the search box are literal characters.
    assert!(find(JobsFilter { search: "%".into(), ..Default::default() }).is_empty());
    assert_eq!(find(JobsFilter { group: "-".into(), ..Default::default() }).len(), 2);
}

#[test]
fn cancelling_keeps_the_row_and_takes_the_lease_from_a_running_worker() {
    use omni_core::models::JobStage;
    use omni_core::repository::CancelOutcome;
    let (_d, repo) = repo();

    // Waiting: cancelled, row and lease state as specified.
    // Waiting in back-off (not_before set), so the assertion below proves the clear.
    let waiting = job(&repo, 1, JobStatus::Pending);
    repo.lease_job("host:1:0", 120).unwrap().expect("lease");
    assert!(repo.requeue_after(waiting, "host:1:0", chrono::Duration::minutes(5), "E_TEST", "later").unwrap());
    assert!(repo.get_job(waiting).unwrap().unwrap().not_before.is_some());
    assert_eq!(repo.cancel_job(waiting).unwrap(), CancelOutcome::Cancelled);
    let row = repo.get_job(waiting).unwrap().expect("the row is kept");
    assert_eq!(row.status, JobStatus::Cancelled);
    assert!(row.lease_owner.is_none() && row.not_before.is_none() && row.completed_at.is_some());
    assert_eq!(repo.cancel_job(waiting).unwrap(), CancelOutcome::NotCancellable, "twice");

    // Running in DOWNLOAD: the worker loses everything it needs.
    let running = job(&repo, 2, JobStatus::Pending);
    let leased = repo.lease_job("host:1:0", 120).unwrap().expect("lease");
    assert_eq!(leased.id, running);
    assert!(repo.set_stage(running, "host:1:0", JobStage::Download).unwrap());
    assert_eq!(repo.cancel_job(running).unwrap(), CancelOutcome::Cancelled);
    assert!(!repo.owns_lease(running, "host:1:0").unwrap());
    assert!(!repo.set_stage(running, "host:1:0", JobStage::Probe).unwrap());
    assert!(!repo.finish(running, "host:1:0", JobStatus::Completed, None, None, None).unwrap());
    let row = repo.get_job(running).unwrap().unwrap();
    assert_eq!(row.status, JobStatus::Cancelled);
    assert_eq!(row.stage, JobStage::Download, "the stage where it stopped is kept");

    // A cancelled job can be queued again.
    assert!(repo.retry_job(running, None).unwrap());
    assert_eq!(repo.get_job(running).unwrap().unwrap().status, JobStatus::Pending);
}

#[test]
fn a_job_that_is_delivering_or_settled_is_not_cancelled() {
    use omni_core::models::JobStage;
    use omni_core::repository::CancelOutcome;
    let (_d, repo) = repo();

    let delivering = job(&repo, 1, JobStatus::Pending);
    repo.lease_job("host:1:0", 120).unwrap().expect("lease");
    assert!(repo.set_stage(delivering, "host:1:0", JobStage::Deliver).unwrap());
    assert_eq!(repo.cancel_job(delivering).unwrap(), CancelOutcome::TooLate);
    let row = repo.get_job(delivering).unwrap().unwrap();
    assert_eq!((row.status, row.stage), (JobStatus::Running, JobStage::Deliver));
    assert!(repo.owns_lease(delivering, "host:1:0").unwrap(), "the worker keeps its lease");
    // Past delivery the file is in Dalet already: a CANCELLED row would lie.
    assert!(repo.set_stage(delivering, "host:1:0", JobStage::Archive).unwrap());
    assert_eq!(repo.cancel_job(delivering).unwrap(), CancelOutcome::TooLate);

    let review = job(&repo, 2, JobStatus::RequiresReview);
    let done = job(&repo, 3, JobStatus::Completed);
    assert_eq!(repo.cancel_job(review).unwrap(), CancelOutcome::NotCancellable);
    assert_eq!(repo.cancel_job(done).unwrap(), CancelOutcome::NotCancellable);
    assert_eq!(repo.cancel_job(99_999).unwrap(), CancelOutcome::NotCancellable);
    assert_eq!(repo.get_job(done).unwrap().unwrap().status, JobStatus::Completed);
}

#[test]
fn the_delivered_file_is_recorded_by_the_lease_holder_without_a_status() {
    let (_d, repo) = repo();
    let id = job(&repo, 1, JobStatus::Pending);
    repo.lease_job("host:1:0", 120).unwrap().expect("lease");
    assert!(!repo.set_delivered_file(id, "someone-else", "W:/x.mxf", 12.5).unwrap());
    assert!(repo.set_delivered_file(id, "host:1:0", "W:/x.mxf", 12.5).unwrap());
    let row = repo.get_job(id).unwrap().unwrap();
    assert_eq!((row.status, row.file_path.as_deref(), row.duration_secs), (JobStatus::Running, Some("W:/x.mxf"), 12.5));

    // After a cancel the row is not touched.
    assert_eq!(repo.cancel_job(id).unwrap(), omni_core::repository::CancelOutcome::Cancelled);
    assert!(!repo.set_delivered_file(id, "host:1:0", "W:/y.mxf", 1.0).unwrap());
    assert_eq!(repo.get_job(id).unwrap().unwrap().file_path.as_deref(), Some("W:/x.mxf"));
}

/// Test-only: move a job's timestamp column to `hours` ago.
fn age(dir: &tempfile::TempDir, id: i64, column: &str, hours: i64) {
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    let t = omni_core::timestamps::format(chrono::Utc::now() - chrono::Duration::hours(hours));
    conn.execute(&format!("UPDATE queue SET {column} = ? WHERE id = ?"), rusqlite::params![t, id]).unwrap();
}

fn review_ids(repo: &Repository, f: &JobsFilter) -> Vec<i64> {
    repo.list_jobs_page(JobsView::Review, f, 1, 20).unwrap().jobs.into_iter().map(|j| j.id).collect()
}

#[test]
fn the_review_view_can_be_searched_and_filtered_by_journalist() {
    let (_d, repo) = repo();
    let mine = job(&repo, 1, JobStatus::RequiresReview);
    let other = repo
        .enqueue(&NewJob::new("https://example.gr/seismos", "2_NIKOLAOU_SEISMOS", "NIKOLAOU"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    repo.lease_job("host:1:0", 120).unwrap().unwrap();
    repo.finish(other, "host:1:0", JobStatus::RequiresReview, None, None, None).unwrap();

    assert_eq!(review_ids(&repo, &JobsFilter { search: "seism".into(), ..Default::default() }), vec![other]);
    assert_eq!(review_ids(&repo, &JobsFilter { journalist: "papadaki".into(), ..Default::default() }), vec![mine]);
    assert!(review_ids(&repo, &JobsFilter { search: "seism".into(), journalist: "PAPADAKI".into(), ..Default::default() }).is_empty());
}

#[test]
fn old_review_jobs_can_be_hidden_and_shown_again_and_nothing_is_deleted() {
    let (d, repo) = repo();
    let old = job(&repo, 1, JobStatus::RequiresReview);
    let fresh = job(&repo, 2, JobStatus::Failed);
    let done = job(&repo, 3, JobStatus::Completed);
    age(&d, old, "updated_at", 100);
    age(&d, done, "updated_at", 100);

    assert_eq!(repo.hide_old_review(72).unwrap(), 1, "only the old review row");
    assert_eq!(review_ids(&repo, &JobsFilter::default()), vec![fresh]);
    assert_eq!(review_ids(&repo, &JobsFilter { include_hidden: true, ..Default::default() }), vec![fresh, old]);
    assert!(repo.get_job(old).unwrap().unwrap().cleared_at.is_some());
    assert!(repo.get_job(done).unwrap().unwrap().cleared_at.is_none(), "delivered jobs are not touched");

    let c = repo.job_counts().unwrap();
    assert_eq!((c.review, c.review_hidden), (1, 1));
    assert_eq!(repo.hide_old_review(72).unwrap(), 0, "twice");
}

#[test]
fn a_hidden_job_that_is_retried_or_marked_done_is_no_longer_hidden() {
    let (d, repo) = repo();
    let a = job(&repo, 1, JobStatus::RequiresReview);
    let b = job(&repo, 2, JobStatus::RequiresReview);
    age(&d, a, "updated_at", 100);
    age(&d, b, "updated_at", 100);
    assert_eq!(repo.hide_old_review(72).unwrap(), 2);

    assert!(repo.retry_job(a, None).unwrap());
    assert!(repo.get_job(a).unwrap().unwrap().cleared_at.is_none());
    assert!(repo.mark_completed_manually(b).unwrap());
    assert!(repo.get_job(b).unwrap().unwrap().cleared_at.is_none());
    // The marked job shows on the live queue like any other delivery.
    assert!(ids(&repo, JobsView::Live, 1, 20).0.contains(&b));
    assert_eq!(repo.job_counts().unwrap().review_hidden, 0);
}

#[test]
fn the_completed_badge_counts_only_todays_deliveries() {
    let (d, repo) = repo();
    let today = job(&repo, 1, JobStatus::Completed);
    let yesterday = job(&repo, 2, JobStatus::Completed);
    age(&d, yesterday, "completed_at", 36);
    let _ = today;

    let c = repo.job_counts().unwrap();
    assert_eq!((c.completed, c.completed_today), (2, 1));
}

#[test]
fn the_queue_summary_does_not_count_a_cancelled_video_as_running() {
    let (_d, repo) = repo();
    let to_run = job(&repo, 1, JobStatus::Pending);
    let cancelled = job(&repo, 2, JobStatus::Pending);
    job(&repo, 3, JobStatus::Pending);
    assert_eq!(repo.cancel_job(cancelled).unwrap(), omni_core::repository::CancelOutcome::Cancelled);
    // The oldest ready job is leased: now running.
    assert_eq!(repo.lease_job("host:1:0", 120).unwrap().unwrap().id, to_run);

    let s = repo.queue_summary().unwrap();
    assert_eq!((s.pending, s.running, s.cancelled, s.total), (1, 1, 1, 3));
}

#[test]
fn a_hidden_attachment_job_is_shown_again_once_its_file_is_attached() {
    let (_d, repo) = repo();
    let id = job(&repo, 1, JobStatus::ManualDownload);
    assert_eq!(repo.hide_old_review(0).unwrap(), 1);
    assert!(repo.attach_source(id, "W:/temp/a.mp4").unwrap());
    assert!(repo.get_job(id).unwrap().unwrap().cleared_at.is_none());
}

#[test]
fn today_starts_at_the_boundary_given_not_at_utc_midnight() {
    let (d, repo) = repo();
    let before = job(&repo, 1, JobStatus::Completed);
    let after = job(&repo, 2, JobStatus::Completed);
    let boundary = chrono::Utc::now() - chrono::Duration::hours(5);
    let set = |id: i64, at: chrono::DateTime<chrono::Utc>| {
        let conn = rusqlite::Connection::open(d.path().join("omni.db")).unwrap();
        conn.execute(
            "UPDATE queue SET completed_at = ? WHERE id = ?",
            rusqlite::params![omni_core::timestamps::format(at), id],
        )
        .unwrap();
    };
    set(before, boundary - chrono::Duration::minutes(1));
    set(after, boundary + chrono::Duration::minutes(1));
    assert_eq!(repo.job_counts_since(boundary).unwrap().completed_today, 1);
    assert_eq!(repo.job_counts_since(boundary - chrono::Duration::minutes(2)).unwrap().completed_today, 2);
    assert_eq!(repo.job_counts_since(boundary + chrono::Duration::minutes(2)).unwrap().completed_today, 0);
}
