//! The repair ladder's records (plan P10.11a): attempts per job, the owner-checked
//! extraction method, and the duplicate rule for a job waiting for approval.

use anyhow::Result;
use chrono::{Duration, Utc};
use tempfile::TempDir;

use omni_core::models::{Enqueued, JobStatus, NewJob};
use omni_core::repository::{AttemptEnd, Repository, DEFAULT_DEDUP_WINDOW_HOURS};

fn repo(dir: &TempDir) -> Repository {
    Repository::new(dir.path().join("omni.db")).unwrap()
}

fn queue(repo: &Repository, url: &str, n: u32) -> Enqueued {
    queue_for(repo, url, n, "ANNA")
}

fn queue_for(repo: &Repository, url: &str, n: u32, journalist: &str) -> Enqueued {
    repo.enqueue(&NewJob::new(url, &format!("{n}_{journalist}_TEST"), journalist), DEFAULT_DEDUP_WINDOW_HOURS).unwrap()
}

/// A re-sent link while the first copy waits for MCR must not become a second
/// job for the same journalist; another journalist needs their own slug.
#[test]
fn a_link_sent_again_while_its_first_copy_awaits_approval_is_a_duplicate_for_the_same_journalist() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let url = "https://news.example/story-1";
    let first = queue(&repo, url, 1).job_id();
    repo.lease_job("w", 180)?.expect("leased");
    assert!(repo.finish(first, "w", JobStatus::AwaitingApproval, None, None, None)?);

    match queue(&repo, url, 2) {
        Enqueued::DuplicateActive { existing_id } => assert_eq!(existing_id, first),
        other => panic!("expected a duplicate of the waiting job, got {other:?}"),
    }
    match queue_for(&repo, url, 3, "anna") {
        Enqueued::DuplicateActive { existing_id } => assert_eq!(existing_id, first, "same journalist, any letter case"),
        other => panic!("expected a duplicate, got {other:?}"),
    }
    assert!(
        matches!(queue_for(&repo, url, 4, "PETROU"), Enqueued::Created { .. }),
        "another journalist is not blocked by a hold that is not theirs"
    );
    // A review-state job is still not an active one: the same link may be queued again.
    let second_url = "https://news.example/story-2";
    let review = queue(&repo, second_url, 5).job_id();
    assert!(repo.lease_job_by_id(review, "w", 180)?);
    assert!(repo.finish(review, "w", JobStatus::RequiresReview, Some("VIDEO_UNRESOLVED"), None, None)?);
    assert!(matches!(queue(&repo, second_url, 6), Enqueued::Created { .. }), "REQUIRES_REVIEW stays re-queueable");
    Ok(())
}

/// PENDING and RUNNING stay duplicates for every journalist, as before.
#[test]
fn a_queued_or_running_copy_is_a_duplicate_whoever_sends_it() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let url = "https://news.example/story-9";
    let first = queue(&repo, url, 1).job_id();
    assert!(matches!(queue_for(&repo, url, 2, "PETROU"), Enqueued::DuplicateActive { existing_id } if existing_id == first));
    repo.lease_job("w", 180)?.expect("leased");
    assert!(matches!(queue_for(&repo, url, 3, "PETROU"), Enqueued::DuplicateActive { existing_id } if existing_id == first));
    Ok(())
}

#[test]
fn attempts_are_recorded_read_back_and_go_with_their_job() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, "https://news.example/a", 1).job_id();
    assert!(repo.latest_attempt(id)?.is_none());

    let t0 = Utc::now();
    let first = repo.start_attempt(id, "shadow", t0)?;
    let open = repo.latest_attempt(id)?.unwrap();
    assert_eq!((open.id, open.mode.as_str(), open.finished_at, open.outcome.clone()), (first, "shadow", None, None));
    assert_eq!((open.llm_calls, open.tokens_in, open.tokens_out, open.next_index), (0, 0, 0, 0));
    assert!(open.started_at.is_some());

    let second = repo.start_attempt(id, "hold", t0 + Duration::seconds(5))?;
    let end = AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        recipe_hash: Some("ab".repeat(32)),
        recipe_id: Some(7),
        domain: Some("news.example".into()),
        candidates_json: Some("[{\"url\":\"https://cdn.example/a.mp4\"}]".into()),
        held_url: Some("https://cdn.example/a.mp4".into()),
        next_index: 1,
        duration_ms: Some(1234),
        expected_secs: Some(95.0),
        ..Default::default()
    };
    repo.finish_attempt(second, &end, t0 + Duration::seconds(6))?;
    let got = repo.latest_attempt(id)?.unwrap();
    assert_eq!(got.id, second, "the latest attempt is the newest");
    assert_eq!(got.mode, "hold");
    assert_eq!(got.outcome.as_deref(), Some("held"));
    assert_eq!(got.recipe_hash.as_deref(), Some("ab".repeat(32).as_str()));
    assert_eq!((got.recipe_id, got.duration_ms), (Some(7), Some(1234)));
    assert_eq!(got.candidates_json, end.candidates_json);
    assert_eq!((got.held_url.as_deref(), got.next_index), (Some("https://cdn.example/a.mp4"), 1));
    assert!(got.finished_at.is_some());
    assert_eq!(got.expected_secs, Some(95.0));
    assert_eq!((got.image_distance, got.image_frame_secs, got.image_host), (None, None, None), "no comparison yet");

    repo.delete_job(id)?;
    assert!(repo.latest_attempt(id)?.is_none(), "attempts are deleted with their job");
    Ok(())
}

#[test]
fn retention_removes_attempts_after_ninety_days_and_keeps_newer_ones() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, "https://news.example/a", 1).job_id();
    repo.start_attempt(id, "hold", Utc::now() - Duration::days(91))?;
    assert_eq!(repo.purge_repair_attempts(90)?, 1);
    repo.start_attempt(id, "hold", Utc::now() - Duration::days(89))?;
    assert_eq!(repo.purge_repair_attempts(90)?, 0);
    assert!(repo.latest_attempt(id)?.is_some());
    Ok(())
}

#[test]
fn the_extraction_method_is_written_only_by_the_lease_holder() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, "https://news.example/a", 1).job_id();
    assert!(!repo.set_leased_extraction_method(id, "w", Some("repair:recipe:abcd1234"))?, "not leased yet");
    repo.lease_job("w", 180)?.expect("leased");
    assert!(!repo.set_leased_extraction_method(id, "other", Some("repair:recipe:abcd1234"))?, "someone else's lease");
    assert_eq!(repo.get_job(id)?.unwrap().extraction_method, None);
    assert!(repo.set_leased_extraction_method(id, "w", Some("repair:recipe:abcd1234"))?);
    assert_eq!(repo.get_job(id)?.unwrap().extraction_method.as_deref(), Some("repair:recipe:abcd1234"));
    Ok(())
}

/// A job with an attempt that held `url`, parked for approval with a held row.
fn held_attempt(repo: &Repository, owner: &str, id: i64, url: &str) -> i64 {
    let at = repo.start_attempt(id, "hold", Utc::now()).unwrap();
    let end = AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        held_url: Some(url.to_string()),
        ..Default::default()
    };
    repo.finish_attempt(at, &end, Utc::now()).unwrap();
    assert!(repo.hold_output(id, owner, "temp/held/x/clip.mxf", 10, &"ab".repeat(32), Some(9.0), Utc::now() + Duration::hours(24)).unwrap());
    assert!(repo.finish(id, owner, JobStatus::AwaitingApproval, None, None, None).unwrap());
    at
}

fn decision(repo: &Repository, id: i64) -> Option<String> {
    repo.latest_attempt(id).unwrap().unwrap().mcr_decision
}

/// The decision lands on the attempt that held the video, in the same
/// transaction as the decision, and only a rejection bars the address.
#[test]
fn mcr_decisions_are_noted_on_the_attempt_and_only_a_rejection_bars_the_address() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let a = "https://cdn.news.example/v/a.mp4";
    let b = "https://cdn.news.example/v/b.mp4";
    let id = queue(&repo, "https://news.example/story-1", 1).job_id();
    repo.lease_job("w", 180)?.expect("leased");

    // Rejected.
    held_attempt(&repo, "w", id, a);
    assert_eq!(decision(&repo, id), None, "undecided while it waits");
    assert!(repo.reject_held(id, "wrong", "not the story", "desk")?);
    assert_eq!(decision(&repo, id).as_deref(), Some("rejected"));
    assert_eq!(repo.rejected_held_urls(id)?, vec![a.to_string()]);

    // Retry (MCR's "try again" clears error_code); a second hold, expired: not a rejection.
    assert!(repo.retry_job(id, None)?);
    repo.lease_job("w", 180)?.expect("leased again");
    held_attempt(&repo, "w", id, b);
    assert!(repo.expire_held(id, Utc::now())?);
    assert_eq!(decision(&repo, id).as_deref(), Some("expired"));
    assert_eq!(repo.rejected_held_urls(id)?, vec![a.to_string()], "an expired hold is not a rejection");

    // A third hold, approved: not a rejection either. A fourth, rejected: both rejections are listed.
    assert!(repo.retry_job(id, None)?);
    repo.lease_job("w", 180)?.expect("leased again");
    let sha = "ab".repeat(32);
    held_attempt(&repo, "w", id, a);
    assert!(repo.approve_held(id, &sha, "desk")?);
    assert_eq!(decision(&repo, id).as_deref(), Some("approved"));
    assert_eq!(repo.rejected_held_urls(id)?, vec![a.to_string()]);
    Ok(())
}

#[test]
fn a_superseded_hold_and_a_decision_on_another_job_leave_the_attempt_undecided() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, "https://news.example/story-1", 1).job_id();
    repo.lease_job("w", 180)?.expect("leased");
    held_attempt(&repo, "w", id, "https://cdn.news.example/v/a.mp4");
    // MCR retries the job without deciding: the hold is superseded, nothing is rejected.
    assert!(repo.retry_job(id, None)?);
    assert_eq!(decision(&repo, id).as_deref(), Some("superseded"));
    assert!(repo.rejected_held_urls(id)?.is_empty());

    // Another job's rejection does not touch this job's attempt.
    let other = queue(&repo, "https://news.example/story-2", 2).job_id();
    assert!(repo.lease_job_by_id(other, "w", 180)?);
    held_attempt(&repo, "w", other, "https://cdn.news.example/v/z.mp4");
    assert!(repo.reject_held(other, "wrong", "no", "desk")?);
    assert_eq!(decision(&repo, id).as_deref(), Some("superseded"));
    assert!(repo.rejected_held_urls(id)?.is_empty());
    assert_eq!(repo.rejected_held_urls(other)?.len(), 1);
    Ok(())
}

/// Review finding (P10.11a): a retried job's old held attempt stays open, the
/// new run's attempt row never gets its outcome (a failed write), and MCR then
/// rejects the new video: the rejection must not land on the old attempt and
/// bar a video MCR never rejected.
#[test]
fn a_rejection_never_lands_on_the_attempt_of_a_hold_superseded_by_a_retry() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let a = "https://cdn.news.example/v/a.mp4";
    let id = queue(&repo, "https://news.example/story-1", 1).job_id();
    repo.lease_job("w", 180)?.expect("leased");
    held_attempt(&repo, "w", id, a);
    assert!(repo.retry_job(id, None)?);

    // Run 2 holds another video but its attempt row is never finished.
    repo.lease_job("w", 180)?.expect("leased again");
    repo.start_attempt(id, "hold", Utc::now())?;
    assert!(repo.hold_output(id, "w", "temp/held/x/clip.mxf", 10, &"cd".repeat(32), Some(9.0), Utc::now() + Duration::hours(24))?);
    assert!(repo.finish(id, "w", JobStatus::AwaitingApproval, None, None, None)?);
    assert!(repo.reject_held(id, "wrong", "not the story", "desk")?);

    assert!(repo.rejected_held_urls(id)?.is_empty(), "video A was never rejected");
    Ok(())
}

#[test]
fn card_evidence_is_written_only_by_the_lease_holder_and_only_on_that_attempt() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, "https://news.example/a", 1).job_id();
    repo.lease_job("w", 180)?.expect("leased");
    let at = repo.start_attempt(id, "hold", Utc::now())?;
    let other = queue(&repo, "https://news.example/b", 2).job_id();

    assert!(!repo.record_card_evidence(at, id, "someone-else", Some(3), Some(4.5), Some("img.example"))?, "not our lease");
    assert!(!repo.record_card_evidence(at, other, "w", Some(3), Some(4.5), Some("img.example"))?, "another job's attempt id");
    let got = repo.latest_attempt(id)?.unwrap();
    assert_eq!((got.image_distance, got.image_frame_secs, got.image_host), (None, None, None), "refused writes leave NULL");

    assert!(repo.record_card_evidence(at, id, "w", Some(3), Some(4.5), Some("img.example"))?);
    let got = repo.latest_attempt(id)?.unwrap();
    assert_eq!((got.image_distance, got.image_frame_secs, got.image_host.as_deref()), (Some(3), Some(4.5), Some("img.example")));
    // A picture kept without a comparison: host only.
    assert!(repo.record_card_evidence(at, id, "w", None, None, Some("img.example"))?);
    let got = repo.latest_attempt(id)?.unwrap();
    assert_eq!((got.image_distance, got.image_frame_secs), (None, None));

    // Once the job is parked (the lease is gone) nothing more is written.
    assert!(repo.finish(id, "w", JobStatus::AwaitingApproval, None, None, None)?);
    assert!(!repo.record_card_evidence(at, id, "w", Some(1), Some(1.0), Some("late.example"))?);
    assert_eq!(repo.latest_attempt(id)?.unwrap().image_host.as_deref(), Some("img.example"));
    Ok(())
}

#[test]
fn the_latest_held_attempt_skips_later_attempts_that_did_not_hold() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, "https://news.example/a", 1).job_id();
    assert!(repo.latest_held_attempt(id)?.is_none());
    let held = repo.start_attempt(id, "hold", Utc::now())?;
    repo.finish_attempt(held, &AttemptEnd { outcome: Some("held".into()), ..Default::default() }, Utc::now())?;
    let later = repo.start_attempt(id, "hold", Utc::now())?;
    repo.finish_attempt(later, &AttemptEnd { outcome: Some("nothing_verified".into()), ..Default::default() }, Utc::now())?;
    assert_eq!(repo.latest_attempt(id)?.unwrap().id, later);
    assert_eq!(repo.latest_held_attempt(id)?.unwrap().id, held, "the card describes the attempt that held");
    Ok(())
}
