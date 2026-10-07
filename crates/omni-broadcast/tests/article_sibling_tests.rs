//! Several videos behind one submitted link (policy C) against a real queue.

use omni_broadcast::article::{queue_article_siblings, Offered};
use omni_core::models::{JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const ARTICLE: &str = "https://www.portal.example/kosmos/story/1/article";
const X1: &str = "https://x.com/i/status/1900000000000000001";
const X2: &str = "https://x.com/i/status/1900000000000000002";
const X3: &str = "https://x.com/i/status/1900000000000000003";
const RAW: &str = "https://cdn.portal.example/video/related/master.m3u8";

fn leased_article_job() -> (tempfile::TempDir, Repository, omni_core::models::Job) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    let mut j = NewJob::new(ARTICLE, "1_MCR_SEISMOS", "MCR");
    j.keyword = "SEISMOS".into();
    j.priority = 3;
    j.email_message_id = Some("<m1@example.gr>".into());
    j.group_code = Some("NEWS".into());
    repo.enqueue(&j, DEFAULT_DEDUP_WINDOW_HOURS).unwrap();
    let job = repo.lease_job("host:1:1", 180).unwrap().unwrap();
    (dir, repo, job)
}

#[test]
fn platform_videos_become_sibling_jobs_and_raw_streams_are_offered() {
    let (_dir, repo, mut job) = leased_article_job();
    let all = vec![X1.to_string(), X2.to_string(), RAW.to_string(), X3.to_string()];

    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &all);

    // The job being processed is renamed 1 -> 1A, in the row and in memory
    // (the pipeline delivers under the in-memory slug).
    assert_eq!((job.index_str.as_str(), job.slug.as_str()), ("1A", "1A_MCR_SEISMOS"));
    let row = repo.get_job(job.id).unwrap().unwrap();
    assert_eq!(row.slug, "1A_MCR_SEISMOS");

    // Siblings: queued, same journalist/keyword/priority/mail, own URL.
    let mut jobs = repo.get_all_jobs().unwrap();
    jobs.sort_by_key(|j| j.id);
    let siblings: Vec<_> = jobs.iter().filter(|j| j.id != job.id).collect();
    let summary: Vec<(&str, &str, JobStatus)> =
        siblings.iter().map(|j| (j.url.as_str(), j.slug.as_str(), j.status)).collect();
    assert_eq!(
        summary,
        vec![(X2, "1B_MCR_SEISMOS", JobStatus::Pending), (X3, "1C_MCR_SEISMOS", JobStatus::Pending)]
    );
    assert!(siblings.iter().all(|j| j.priority == 3 && j.email_message_id.as_deref() == Some("<m1@example.gr>")));
    // The MCR mail view files them under the article's link (plan P7.6),
    // and they carry the article's group label.
    assert!(siblings.iter().all(|j| j.parent_job_id == Some(job.id)), "{siblings:?}");
    assert!(siblings.iter().all(|j| j.group_code.as_deref() == Some("NEWS")), "{siblings:?}");

    // The raw stream is offered, with the next index in the sequence.
    let offered: Vec<Offered> = serde_json::from_str(row.candidates_json.as_deref().unwrap()).unwrap();
    assert_eq!(offered, vec![Offered { url: RAW.into(), index_str: "1D".into(), queued_job_id: None }]);
}

#[test]
fn a_retried_article_does_not_queue_its_siblings_twice() {
    let (_dir, repo, mut job) = leased_article_job();
    let all = vec![X1.to_string(), X2.to_string()];
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &all);
    let before = repo.get_all_jobs().unwrap().len();

    // Second sniff of the same article (a retry): the sibling is already queued.
    let mut again = repo.get_job(job.id).unwrap().unwrap();
    queue_article_siblings(&repo, "host:1:1", &mut again, X1, &all);
    assert_eq!(repo.get_all_jobs().unwrap().len(), before);
}

#[test]
fn a_hand_set_slug_is_never_renamed() {
    let (_dir, repo, mut job) = leased_article_job();
    // MCR renamed it by hand before it ran.
    job.slug = "SPECIAL_REPORT".into();
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &[X1.to_string(), X2.to_string()]);
    assert_eq!(job.slug, "SPECIAL_REPORT");
    assert_eq!(job.index_str, "1");
}

#[test]
fn a_single_video_article_is_left_exactly_as_it_was() {
    let (_dir, repo, mut job) = leased_article_job();
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &[X1.to_string()]);
    assert_eq!(job.slug, "1_MCR_SEISMOS");
    assert_eq!(repo.get_all_jobs().unwrap().len(), 1);
    assert!(repo.get_job(job.id).unwrap().unwrap().candidates_json.is_none());
}
