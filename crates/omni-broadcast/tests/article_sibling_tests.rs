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

    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &all, &Default::default());

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
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &all, &Default::default());
    let before = repo.get_all_jobs().unwrap().len();

    // Second sniff of the same article (a retry): the sibling is already queued.
    let mut again = repo.get_job(job.id).unwrap().unwrap();
    queue_article_siblings(&repo, "host:1:1", &mut again, X1, &all, &Default::default());
    assert_eq!(repo.get_all_jobs().unwrap().len(), before);
}

#[test]
fn a_hand_set_slug_is_never_renamed() {
    let (_dir, repo, mut job) = leased_article_job();
    // MCR renamed it by hand before it ran.
    job.slug = "SPECIAL_REPORT".into();
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &[X1.to_string(), X2.to_string()], &Default::default());
    assert_eq!(job.slug, "SPECIAL_REPORT");
    assert_eq!(job.index_str, "1");
}

#[test]
fn a_single_video_article_is_left_exactly_as_it_was() {
    let (_dir, repo, mut job) = leased_article_job();
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &[X1.to_string()], &Default::default());
    assert_eq!(job.slug, "1_MCR_SEISMOS");
    assert_eq!(repo.get_all_jobs().unwrap().len(), 1);
    assert!(repo.get_job(job.id).unwrap().unwrap().candidates_json.is_none());
}

#[test]
fn a_cancelled_article_job_queues_no_siblings() {
    // P7.20: MCR cancelled the job while its page was being resolved. The
    // other videos of the article must not be queued for air.
    let (_dir, repo, mut job) = leased_article_job();
    assert_eq!(repo.cancel_job(job.id).unwrap(), omni_core::repository::CancelOutcome::Cancelled);
    let all = vec![X1.to_string(), X2.to_string(), RAW.to_string()];

    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &all, &Default::default());

    let jobs = repo.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1, "siblings were queued for a cancelled job: {jobs:?}");
    assert!(repo.get_job(job.id).unwrap().unwrap().candidates_json.is_none(), "offers were stored too");
}

#[test]
fn a_job_another_worker_holds_queues_no_siblings() {
    let (_dir, repo, mut job) = leased_article_job();
    let all = vec![X1.to_string(), X2.to_string()];

    queue_article_siblings(&repo, "host:1:OTHER", &mut job, X1, &all, &Default::default());

    assert_eq!(repo.get_all_jobs().unwrap().len(), 1);
}

/// P3.14: a 70-minute studio show from a site widget and a clip found outside
/// the article are offered on the job, not queued; the article's own 8 s post
/// is queued. Without the facts all three would be queued.
#[test]
fn a_long_video_and_one_outside_the_article_are_offered_not_queued() {
    use omni_broadcast::article::{facts_key, Facts, VideoFacts};
    let (_dir, repo, mut job) = leased_article_job();
    let yt = "https://www.youtube.com/watch?v=sTuDiOsHoW1";
    let all = vec![X1.to_string(), X2.to_string(), yt.to_string(), X3.to_string()];
    let mut facts = Facts::new();
    facts.insert(facts_key(X2), VideoFacts { duration_secs: Some(8.0), ..Default::default() });
    facts.insert(facts_key(yt), VideoFacts { duration_secs: Some(4200.0), ..Default::default() });
    facts.insert(facts_key(X3), VideoFacts { in_article: Some(false), ..Default::default() });

    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &all, &facts);

    let queued: Vec<String> = repo.get_all_jobs().unwrap().into_iter().filter(|j| j.id != job.id).map(|j| j.url).collect();
    assert_eq!(queued, vec![X2.to_string()]);
    let row = repo.get_job(job.id).unwrap().unwrap();
    let offered: Vec<Offered> = serde_json::from_str(row.candidates_json.as_deref().unwrap()).unwrap();
    let urls: Vec<&str> = offered.iter().map(|o| o.url.as_str()).collect();
    assert_eq!(urls, vec![yt, X3]);
}

#[test]
fn a_retry_never_takes_nor_requeues_a_video_a_previous_attempt_queued_as_a_sibling() {
    use omni_broadcast::article::{queued_children, remaining_after_queued};
    let (_dir, repo, mut job) = leased_article_job();
    let all = vec![X1.to_string(), X2.to_string(), X3.to_string()];
    // Attempt 1 took X1 and queued X2 and X3.
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &all, &Default::default());
    // One sibling has already ended: it still holds its video.
    let ended = repo.get_all_jobs().unwrap().into_iter().find(|j| j.url == X3).unwrap();
    repo.cancel_job(ended.id).unwrap();
    assert_eq!(repo.get_job(ended.id).unwrap().unwrap().status, JobStatus::Cancelled);
    let queued = queued_children(&repo.jobs_with_children(job.id).unwrap(), job.id);
    assert_eq!(queued, vec![X2.to_string(), X3.to_string()], "the parent is not its own child");

    // Order of yt-dlp's scan: the retry's pool is X1 alone.
    assert_eq!(remaining_after_queued(&all, &queued), (vec![X1.to_string()], false));
    // Order of the sniffer's ranking, which now puts a queued video first.
    let ranked = vec![X2.to_string(), X1.to_string(), X3.to_string()];
    assert_eq!(remaining_after_queued(&ranked, &queued), (vec![X1.to_string()], false));
    // Every video queued: nothing is left to take, and that is said.
    let only = vec![X3.to_string(), X2.to_string()];
    assert_eq!(remaining_after_queued(&only, &queued), (Vec::<String>::new(), true));
    assert_eq!(remaining_after_queued(&[], &queued), (Vec::<String>::new(), false));
}

// ------------------------------------------------------------------ site furniture (P3.16)

fn furniture(url: &str, others: usize) -> omni_core::site_videos::Furniture {
    omni_core::site_videos::Furniture { url: url.into(), other_articles: others }
}

fn warns(repo: &Repository, id: i64) -> Vec<String> {
    repo.get_job_events(id, 100).unwrap().into_iter().filter(|e| e.level == "WARN").map(|e| e.message).collect()
}

fn offers(repo: &Repository, id: i64) -> Vec<Offered> {
    repo.get_job(id).unwrap().unwrap().candidates_json.map(|t| serde_json::from_str(&t).unwrap()).unwrap_or_default()
}

#[test]
fn site_furniture_is_offered_with_a_warning_that_says_on_how_many_articles() {
    let (_dir, repo, job) = leased_article_job();
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(RAW, 3)]);
    assert_eq!(offers(&repo, job.id).iter().map(|o| (o.url.as_str(), o.queued_job_id)).collect::<Vec<_>>(), [(RAW, None)]);
    let w = warns(&repo, job.id);
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("shown on 3 other articles of this site this week") && w[0].contains(RAW), "{}", w[0]);
    // Nothing was queued.
    assert_eq!(repo.get_all_jobs().unwrap().len(), 1);
}

#[test]
fn offering_the_same_furniture_again_neither_duplicates_the_offer_nor_the_warning() {
    let (_dir, repo, job) = leased_article_job();
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(RAW, 3)]);
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(RAW, 3)]);
    assert_eq!(offers(&repo, job.id).len(), 1);
    assert_eq!(warns(&repo, job.id).len(), 1);
}

#[test]
fn furniture_and_the_articles_own_offers_are_merged_in_either_order() {
    let (_dir, repo, mut job) = leased_article_job();
    // Furniture first, then the article's siblings plan their offer.
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(RAW, 2)]);
    let other = "https://cdn.portal.example/video/more/master.m3u8";
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &[X1.to_string(), other.to_string()], &Default::default());
    let urls: Vec<String> = offers(&repo, job.id).into_iter().map(|o| o.url).collect();
    assert_eq!(urls, [RAW, other], "neither overwrote the other");
    // And again after the siblings: nothing is added twice.
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(RAW, 2), furniture(other, 2)]);
    assert_eq!(offers(&repo, job.id).len(), 2);
    let idx: Vec<String> = offers(&repo, job.id).into_iter().map(|o| o.index_str).collect();
    assert_ne!(idx[0], idx[1], "two offers never share an index: {idx:?}");

    // The other order: the siblings plan first, then the furniture is offered.
    let (_d2, repo2, mut job2) = leased_article_job();
    queue_article_siblings(&repo2, "host:1:1", &mut job2, X1, &[X1.to_string(), other.to_string(), X2.to_string()], &Default::default());
    omni_broadcast::article::offer_site_wide(&repo2, &job2, &[furniture(RAW, 2)]);
    let mut taken: Vec<String> = offers(&repo2, job2.id).into_iter().map(|o| o.index_str).collect();
    taken.extend(repo2.get_all_jobs().unwrap().into_iter().map(|j| j.index_str));
    let mut sorted = taken.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), taken.len(), "an offer never shares an index with another offer or a queued job: {taken:?}");
}

#[test]
fn the_offered_list_is_capped() {
    let (_dir, repo, job) = leased_article_job();
    let many: Vec<_> = (0..omni_broadcast::article::MAX_OFFERED + 3).map(|i| furniture(&format!("https://cdn.portal.example/v/{i}.mp4"), 2)).collect();
    omni_broadcast::article::offer_site_wide(&repo, &job, &many);
    assert_eq!(offers(&repo, job.id).len(), omni_broadcast::article::MAX_OFFERED);
}

#[test]
fn an_old_offer_that_became_the_primary_or_a_sibling_is_no_longer_offered() {
    let (_dir, repo, mut job) = leased_article_job();
    // An earlier look offered RAW and X2 (X2 is a raw stream here, to be offered rather than queued).
    let other = "https://cdn.portal.example/video/other/master.m3u8";
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(RAW, 2), furniture(other, 2)]);
    assert_eq!(offers(&repo, job.id).len(), 2);
    // The retry's primary is RAW, and a platform sibling X2 is queued.
    queue_article_siblings(&repo, "host:1:1", &mut job, RAW, &[RAW.to_string(), X2.to_string()], &Default::default());
    let urls: Vec<String> = offers(&repo, job.id).into_iter().map(|o| o.url).collect();
    assert_eq!(urls, [other], "RAW is the job's video now, and stays offered no more");
    // A video queued as a sibling is not offered either.
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(X2, 2)]);
    assert!(!offers(&repo, job.id).iter().any(|o| o.url == X2), "X2 is job #{} already", job.id + 1);
}

#[test]
fn a_stored_offer_whose_index_a_planned_sibling_takes_moves_to_a_free_one() {
    let (_dir, repo, mut job) = leased_article_job();
    // An earlier look offered the furniture as "1B".
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(RAW, 2)]);
    assert_eq!(offers(&repo, job.id)[0].index_str, "1B");
    // The retry plans a platform sibling X2: it is "1B" too (the job becomes "1A").
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &[X1.to_string(), X2.to_string()], &Default::default());
    let sibling = repo.get_all_jobs().unwrap().into_iter().find(|j| j.url == X2).unwrap();
    assert_eq!(sibling.index_str, "1B");
    let moved = offers(&repo, job.id);
    assert_eq!(moved.len(), 1);
    assert_ne!(moved[0].index_str, "1B", "the offer no longer shares the sibling's index");
    assert_ne!(moved[0].index_str, job.index_str);
}

/// An offer MCR already queued keeps its label: its child job carries that index.
#[test]
fn an_offer_mcr_already_queued_keeps_its_index_when_a_sibling_is_planned() {
    let (_dir, repo, mut job) = leased_article_job();
    omni_broadcast::article::offer_site_wide(&repo, &job, &[furniture(RAW, 2)]);
    // MCR queued it as job #999.
    let stored = repo.get_job(job.id).unwrap().unwrap().candidates_json.unwrap();
    let mut list: serde_json::Value = serde_json::from_str(&stored).unwrap();
    list[0]["queued_job_id"] = serde_json::json!(999);
    repo.set_candidates(job.id, Some(&list.to_string())).unwrap();
    queue_article_siblings(&repo, "host:1:1", &mut job, X1, &[X1.to_string(), X2.to_string()], &Default::default());
    let kept = offers(&repo, job.id);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].index_str, "1B", "a queued offer's label matches its child job");
}
