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
