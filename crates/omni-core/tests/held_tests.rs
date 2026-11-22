//! A video held for MCR's approval (plan P10.5): the queue side.
//!
//! Real SQLite files; the file side (hashing, sweep, directories) is covered
//! in `held.rs` and the pipeline side in `omni-broadcast`.

use anyhow::Result;
use chrono::{Duration, Utc};
use tempfile::TempDir;

use omni_core::models::{JobStatus, JobsFilter, JobsView, LateAfter, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn repo(dir: &TempDir) -> Repository {
    Repository::new(dir.path().join("omni.db")).unwrap()
}

fn queue(repo: &Repository, n: u32) -> i64 {
    repo.enqueue(
        &NewJob::new(&format!("https://example.gr/v{n}"), &format!("{n}_ANNA_TEST"), "ANNA"),
        DEFAULT_DEDUP_WINDOW_HOURS,
    )
    .unwrap()
    .job_id()
}

/// A job leased by "w", held, and parked as AWAITING_APPROVAL.
fn held(repo: &Repository, n: u32) -> i64 {
    let id = queue(repo, n);
    let leased = repo.lease_job("w", 180).unwrap().expect("leased");
    assert_eq!(leased.id, id);
    assert!(repo.hold_output(id, "w", "temp/held/x/clip.mxf", 1000, SHA, Some(12.5), Utc::now() + Duration::hours(24)).unwrap());
    assert!(repo.finish(id, "w", JobStatus::AwaitingApproval, None, None, None).unwrap());
    id
}

#[test]
fn a_held_job_is_parked_not_terminal_and_never_leased() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);

    let job = repo.get_job(id)?.unwrap();
    assert_eq!(job.status, JobStatus::AwaitingApproval);
    assert_eq!(job.status.as_str(), "AWAITING_APPROVAL");
    assert_eq!(JobStatus::parse("AWAITING_APPROVAL"), Some(JobStatus::AwaitingApproval));
    assert!(!job.status.is_terminal());
    assert_eq!(job.lease_owner, None);
    assert_eq!(job.completed_at, None, "a parked job is not finished");

    assert!(repo.lease_job("w2", 180)?.is_none(), "the lease query must never take an AWAITING_APPROVAL job");
    assert_eq!(repo.reap_expired_leases()?, (0, 0));
    assert_eq!(repo.recover_on_startup("w")?, 0);
    assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval);
    Ok(())
}

#[test]
fn the_queue_summary_counts_held_jobs_and_the_review_view_leaves_them_out() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    assert_eq!(repo.queue_summary(LateAfter::default())?.approval, 0);
    held(&repo, 1);
    let waiting = queue(&repo, 2);
    let s = repo.queue_summary(LateAfter::default())?;
    assert_eq!((s.approval, s.pending, s.total), (1, 1, 2));
    let _ = waiting;

    let filter = JobsFilter::default();
    let review = repo.list_jobs_page(JobsView::Review, &filter, 1, 20)?;
    assert_eq!(review.total, 0, "a held job is not in the review list");
    assert_eq!(repo.job_counts(LateAfter::default())?.review, 0);
    Ok(())
}

#[test]
fn approve_needs_the_right_sha_and_works_once() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);

    assert!(!repo.approve_held(id, &"0".repeat(64), "mcr")?, "a different file is not approved");
    let h = repo.get_held(id)?.unwrap();
    assert_eq!(h.decision, None);
    assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval);

    assert!(repo.approve_held(id, SHA, "desk 1")?);
    let h = repo.get_held(id)?.unwrap();
    assert_eq!((h.decision.as_deref(), h.decided_by.as_deref()), (Some("approved"), Some("desk 1")));
    assert!(h.decided_at.is_some());
    let job = repo.get_job(id)?.unwrap();
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!((job.attempts, job.not_before, job.cleared_at, job.lease_owner), (0, None, None, None));
    assert!(repo.approved_held(id)?.is_some());
    assert!(repo.waiting_held(id)?.is_none());

    assert!(!repo.approve_held(id, SHA, "desk 2")?, "a second approval is refused");
    assert_eq!(repo.get_held(id)?.unwrap().decided_by.as_deref(), Some("desk 1"));

    // The approved job is leasable again, and only now.
    let leased = repo.lease_job("w", 180)?.expect("approved job leases");
    assert_eq!(leased.id, id);
    Ok(())
}

#[test]
fn approve_is_all_or_nothing_when_the_job_is_not_waiting() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);
    // MCR cancelled it: the row still says waiting until the sweep runs.
    assert_eq!(repo.cancel_job(id)?, omni_core::repository::CancelOutcome::Cancelled);
    assert!(!repo.approve_held(id, SHA, "mcr")?);
    assert_eq!(repo.get_held(id)?.unwrap().decision, None, "the half that could apply must roll back");
    assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::Cancelled);
    Ok(())
}

#[test]
fn reject_sends_the_job_to_review_with_its_code_and_only_from_awaiting() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);
    let other = queue(&repo, 2);

    assert!(!repo.reject_held(other, "wrong_video", "msg", "mcr")?, "a job that is not held cannot be rejected");
    assert_eq!(repo.get_job(other)?.unwrap().status, JobStatus::Pending);

    assert!(repo.reject_held(id, "wrong_video", "Το MCR απέρριψε το βίντεο.", "desk 1")?);
    let job = repo.get_job(id)?.unwrap();
    assert_eq!(job.status, JobStatus::RequiresReview);
    assert_eq!(job.error_code.as_deref(), Some("REJECTED_BY_MCR"));
    assert_eq!(job.error_message.as_deref(), Some("Το MCR απέρριψε το βίντεο."));
    let h = repo.get_held(id)?.unwrap();
    assert_eq!((h.decision.as_deref(), h.reason.as_deref()), (Some("rejected"), Some("wrong_video")));
    assert!(!repo.reject_held(id, "other", "msg", "desk 2")?, "a second decision is refused");
    // Now in the review list.
    assert_eq!(repo.list_jobs_page(JobsView::Review, &JobsFilter::default(), 1, 20)?.total, 1);
    Ok(())
}

#[test]
fn the_action_routes_still_work_on_a_held_job() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let retried = held(&repo, 1);
    let cancelled = held(&repo, 2);
    let marked = held(&repo, 3);
    assert!(repo.retry_job(retried, None)?);
    assert_eq!(repo.get_job(retried)?.unwrap().status, JobStatus::Pending);
    assert_eq!(repo.cancel_job(cancelled)?, omni_core::repository::CancelOutcome::Cancelled);
    assert!(repo.mark_completed_manually(marked)?);
    assert_eq!(repo.get_job(marked)?.unwrap().status, JobStatus::CompletedManual);
    Ok(())
}

#[test]
fn a_retry_supersedes_an_approved_hold_so_it_cannot_be_delivered_for_a_new_link() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);
    assert!(repo.approve_held(id, SHA, "mcr")?);
    // Approved but not yet leased: MCR gives the job another link.
    assert!(repo.retry_job(id, Some("https://example.gr/other"))?);
    assert!(repo.approved_held(id)?.is_none(), "the old file must not be delivered for the new link");
    assert_eq!(repo.get_held(id)?.unwrap().decision.as_deref(), Some("superseded"));
    Ok(())
}

#[test]
fn hold_output_is_written_only_for_the_lease_holder() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, 1);
    repo.lease_job("w", 180)?.unwrap();
    let later = Utc::now() + Duration::hours(24);

    assert!(!repo.hold_output(id, "someone-else", "p", 1, SHA, None, later)?);
    assert!(repo.get_held(id)?.is_none(), "a worker that lost the job writes nothing");

    assert!(repo.hold_output(id, "w", "p", 1, SHA, Some(3.0), later)?);
    let h = repo.get_held(id)?.unwrap();
    assert_eq!((h.bytes, h.sha256.as_str(), h.duration, h.decision), (1, SHA, Some(3.0), None));
    assert!(h.created_at.is_some() && h.expires_at.is_some());

    // Replaced, not duplicated, when the job holds again.
    assert!(repo.hold_output(id, "w", "p2", 2, SHA, Some(3.0), later)?);
    assert_eq!(repo.get_held(id)?.unwrap().path, "p2");

    // Once the lease is gone, nothing is written.
    repo.finish(id, "w", JobStatus::RequiresReview, None, None, None)?;
    assert!(!repo.hold_output(id, "w", "p3", 3, SHA, None, later)?);
    assert_eq!(repo.get_held(id)?.unwrap().path, "p2");
    Ok(())
}

#[test]
fn the_hold_budget_counts_waiting_and_approved_rows_but_not_the_jobs_own() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let a = held(&repo, 1);
    let b = held(&repo, 2);
    assert_eq!(repo.held_bytes_in_use(999)?, 2000);
    assert_eq!(repo.held_bytes_in_use(a)?, 1000, "the job's own earlier hold is replaced, not added");
    assert!(repo.approve_held(b, SHA, "mcr")?);
    assert_eq!(repo.held_bytes_in_use(999)?, 2000, "approved but undelivered still occupies the disk");
    assert!(repo.mark_held(b, "delivered")?);
    assert_eq!(repo.held_bytes_in_use(999)?, 1000);
    Ok(())
}

#[test]
fn expiry_moves_only_a_waiting_job() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);
    assert!(repo.expire_held(id, Utc::now())?);
    let job = repo.get_job(id)?.unwrap();
    assert_eq!((job.status, job.error_code.as_deref()), (JobStatus::RequiresReview, Some("APPROVAL_EXPIRED")));
    assert!(!repo.expire_held(id, Utc::now())?, "already decided");
    Ok(())
}

#[test]
fn the_finish_of_a_held_job_is_owner_checked() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, 1);
    repo.lease_job("w", 180)?.unwrap();
    assert!(!repo.finish(id, "intruder", JobStatus::AwaitingApproval, None, None, None)?);
    assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::Running);
    Ok(())
}

#[test]
fn a_hold_and_an_approval_clear_a_stale_file_path() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = queue(&repo, 1);
    repo.lease_job("w", 180)?.unwrap();
    assert!(repo.set_delivered_file(id, "w", "C:/old/x.mxf", 1.0)?);
    assert!(repo.hold_output(id, "w", "p", 1, SHA, None, Utc::now() + Duration::hours(1))?);
    assert_eq!(repo.get_job(id)?.unwrap().file_path, None, "a held run delivered nothing");

    // The same on approval, for a row that somehow got a path meanwhile.
    assert!(repo.finish(id, "w", JobStatus::AwaitingApproval, None, None, None)?);
    repo.update_job_status(id, JobStatus::AwaitingApproval, None, Some("C:/stale.mxf"), None)?;
    assert!(repo.approve_held(id, SHA, "mcr")?);
    assert_eq!(repo.get_job(id)?.unwrap().file_path, None);
    Ok(())
}

#[test]
fn a_delivery_is_recorded_and_finished_only_by_the_lease_holder() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);
    assert!(repo.approve_held(id, SHA, "mcr")?);
    repo.lease_job("w", 180)?.unwrap();

    // Not ours: nothing recorded, nothing finished.
    assert!(!repo.record_held_delivery(id, "intruder", "wf/a.mxf", 2.0)?);
    assert_eq!(repo.get_held(id)?.unwrap().delivered_path, None);
    // Not recorded yet: finishing would complete a job with no delivery.
    assert!(!repo.finish_held_delivery(id, "w")?);
    assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::Running);

    assert!(repo.record_held_delivery(id, "w", "wf/a.mxf", 2.0)?);
    assert!(!repo.finish_held_delivery(id, "intruder")?, "another worker cannot finish it");
    assert_eq!(repo.get_held(id)?.unwrap().decision.as_deref(), Some("approved"));

    assert!(repo.finish_held_delivery(id, "w")?);
    let job = repo.get_job(id)?.unwrap();
    assert_eq!((job.status, job.file_path.as_deref()), (JobStatus::Completed, Some("wf/a.mxf")));
    assert!(job.delivered_at.is_some());
    assert_eq!(repo.get_held(id)?.unwrap().decision.as_deref(), Some("delivered"));
    Ok(())
}

#[test]
fn a_file_path_that_is_not_this_approvals_delivery_does_not_complete_the_job() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);
    assert!(repo.approve_held(id, SHA, "mcr")?);
    repo.lease_job("w", 180)?.unwrap();
    assert!(repo.set_delivered_file(id, "w", "wf/stale.mxf", 1.0)?);
    assert!(!repo.finish_held_delivery(id, "w")?, "a stale path is not a delivery of the held file");
    assert_eq!(repo.get_held(id)?.unwrap().decision.as_deref(), Some("approved"));
    Ok(())
}

#[test]
fn an_approved_hold_can_be_leased_on_its_own() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let waiting = held(&repo, 2);
    let plain = queue(&repo, 1);
    assert!(repo.lease_approved_held_job("w", 180)?.is_none(), "a plain pending job is not an approved hold");
    assert!(repo.approve_held(waiting, SHA, "mcr")?);
    let job = repo.lease_approved_held_job("w", 180)?.expect("the approved hold leases");
    assert_eq!(job.id, waiting);
    assert_eq!(repo.get_job(plain)?.unwrap().status, JobStatus::Pending, "the plain job is left for the normal lease");
    assert!(repo.lease_approved_held_job("w2", 180)?.is_none());
    Ok(())
}

#[test]
fn a_job_with_a_waiting_or_approved_hold_cannot_be_renamed() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 2);
    let plain = queue(&repo, 1);
    assert!(repo.rename_job_keyword(plain, "NEWNAME")?.is_some(), "an ordinary pending job renames");

    assert!(repo.rename_job_keyword(id, "NEWNAME")?.is_none(), "waiting");
    assert!(repo.approve_held(id, SHA, "mcr")?);
    assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::Pending);
    assert!(repo.rename_job_keyword(id, "NEWNAME")?.is_none(), "approved, pending: the file keeps its name");
    assert_eq!(repo.get_job(id)?.unwrap().slug, "2_ANNA_TEST");
    Ok(())
}

#[test]
fn lease_job_by_id_takes_only_that_pending_job() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let other = queue(&repo, 1); // older and first in line
    let mine = queue(&repo, 2);

    assert!(repo.lease_job_by_id(mine, "dev", 60)?);
    assert_eq!(repo.get_job(mine)?.unwrap().status, JobStatus::Running);
    assert_eq!(repo.get_job(mine)?.unwrap().lease_owner.as_deref(), Some("dev"));
    assert_eq!(repo.get_job(other)?.unwrap().status, JobStatus::Pending, "another pending job is never taken");

    // Already running: not leasable again. A missing id: false.
    assert!(!repo.lease_job_by_id(mine, "dev2", 60)?);
    assert!(!repo.lease_job_by_id(9999, "dev", 60)?);
    // Not pending at all (cancelled): false, and still cancelled.
    repo.cancel_job(other)?;
    assert!(!repo.lease_job_by_id(other, "dev", 60)?);
    assert_eq!(repo.get_job(other)?.unwrap().status, JobStatus::Cancelled);
    Ok(())
}

#[test]
fn only_an_approved_hold_keeps_a_job_out_of_the_archive_stage_recovery() -> Result<()> {
    use omni_core::models::JobStage;
    let dir = TempDir::new()?;
    let repo = repo(&dir);

    // Held, then superseded by a retry, then delivered the normal way and
    // crashed in ARCHIVE: provably on air, so finished, not requeued.
    let superseded = held(&repo, 1);
    assert!(repo.retry_job(superseded, None)?);
    let leased = repo.lease_job("w:1:0", 3600)?.expect("leased");
    assert_eq!(leased.id, superseded);
    assert!(repo.set_stage(superseded, "w:1:0", JobStage::Archive)?);

    // An approved hold at ARCHIVE keeps its own idempotent re-delivery path.
    let approved = held(&repo, 2);
    assert!(repo.approve_held(approved, SHA, "mcr")?);
    let leased = repo.lease_approved_held_job("w:1:1", 3600)?.expect("approved job leased");
    assert_eq!(leased.id, approved);
    assert!(repo.set_stage(approved, "w:1:1", JobStage::Archive)?);

    repo.recover_on_startup("w")?;

    assert_eq!(repo.get_job(superseded)?.unwrap().status, JobStatus::Completed);
    assert_eq!(repo.get_job(approved)?.unwrap().status, JobStatus::Pending);
    Ok(())
}

fn expires(repo: &Repository, id: i64) -> chrono::DateTime<Utc> {
    repo.get_held(id).unwrap().unwrap().expires_at.expect("an expiry")
}

/// Within a second: the stored times are milliseconds, `now` is finer.
fn about(a: chrono::DateTime<Utc>, b: chrono::DateTime<Utc>) -> bool {
    (a - b).num_milliseconds().abs() <= 1000
}

/// P11.10: a tick counts against the hold only when a desk was connected over
/// it and it was no longer than the gap; otherwise the expiry moves on by it.
/// A decided row is never touched.
#[test]
fn the_approval_clock_runs_only_while_a_desk_was_connected() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let id = held(&repo, 1);
    let decided = held(&repo, 2);
    assert!(repo.reject_held(decided, "not_usable", "no", "desk")?);
    let (e0, d0) = (expires(&repo, id), repo.get_held(decided)?.unwrap().expires_at);
    let now = Utc::now();
    let gap = Duration::minutes(3);

    assert_eq!(repo.account_held_clocks(now + Duration::minutes(1), true, gap)?, 0, "watched: the clock ran");
    assert_eq!(expires(&repo, id), e0);

    assert_eq!(repo.account_held_clocks(now + Duration::minutes(2), false, gap)?, 1, "no desk: the expiry moves");
    assert!(about(expires(&repo, id), e0 + Duration::minutes(1)), "{} vs {}", expires(&repo, id), e0);

    // The daemon was stopped for 10 hours: nobody saw it, whatever the desk says now.
    assert_eq!(repo.account_held_clocks(now + Duration::minutes(2) + Duration::hours(10), true, gap)?, 1);
    assert!(about(expires(&repo, id), e0 + Duration::minutes(1) + Duration::hours(10)));

    assert_eq!(repo.get_held(decided)?.unwrap().expires_at, d0, "a decided row is left");
    Ok(())
}

/// P11.10: a held video nobody's desk saw does not expire; once seen for the
/// whole hold, it expires into review as before.
#[test]
fn an_unseen_held_video_does_not_expire_and_a_seen_one_ends_in_review() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let temp = dir.path().join("temp");
    let id = held(&repo, 1);
    let now = Utc::now();

    // 30 hours with no desk: past the original 24 h, still waiting.
    repo.account_held_clocks(now + Duration::hours(30), false, Duration::minutes(3))?;
    let report = omni_core::held::sweep(&repo, &temp, now + Duration::hours(30))?;
    assert!(report.expired.is_empty(), "{report:?}");
    assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval);

    // Then a desk watches it for the whole hold (one long tick, the gap allowing it).
    let later = now + Duration::hours(30) + Duration::hours(24) + Duration::minutes(1);
    assert_eq!(repo.account_held_clocks(later, true, Duration::hours(25))?, 0);
    let report = omni_core::held::sweep(&repo, &temp, later)?;
    assert_eq!(report.expired, vec![id]);
    let job = repo.get_job(id)?.unwrap();
    assert_eq!((job.status, job.error_code.as_deref()), (JobStatus::RequiresReview, Some("APPROVAL_EXPIRED")));
    Ok(())
}

/// P11.10: the desk's reminder reads how many wait, the oldest and the soonest expiry.
#[test]
fn the_reminder_counts_the_videos_waiting_for_approval() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    assert_eq!(repo.held_waiting()?.count, 0);
    let a = held(&repo, 1);
    let b = held(&repo, 2);
    assert!(repo.reject_held(b, "not_usable", "no", "desk")?);
    held(&repo, 3);
    let w = repo.held_waiting()?;
    assert_eq!(w.count, 2);
    assert_eq!(w.oldest_held_at, repo.get_held(a)?.unwrap().created_at);
    assert_eq!(w.soonest_expires_at, Some(expires(&repo, a)));
    Ok(())
}

/// P11.10b: when the clocks could not be accounted, the tick expires nothing
/// (a video past its stored time stays waiting); the next good tick does.
/// `reaper_tick` is what the daemon's reaper calls.
#[test]
fn a_tick_whose_clocks_failed_expires_nothing() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let temp = dir.path().join("temp");
    let id = held(&repo, 1);
    let late = Utc::now() + Duration::hours(25);

    let report = omni_core::held::sweep_after_clocks(&repo, &temp, late, Err(anyhow::anyhow!("database locked")))?;
    assert!(report.expired.is_empty(), "{report:?}");
    assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval);

    // A good tick with a desk watching, over a gap it may count.
    let desks = omni_core::held::DeskPresence::new();
    let _desk = desks.connect();
    let report = omni_core::held::reaper_tick(&repo, &temp, &desks, late, Duration::hours(26))?;
    assert_eq!(report.expired, vec![id]);
    Ok(())
}

/// P14.6: the desk's page reads its held and repair details in one batch, and
/// gets what the per-job queries give: the waiting held video and the latest
/// HELD attempt of a job awaiting approval (not a later attempt that did not
/// hold), the latest attempt of a completed job, nothing for the others.
#[test]
fn the_desk_batch_gives_what_the_per_job_queries_give() -> Result<()> {
    use omni_core::repository::AttemptEnd;
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let now = Utc::now();
    let end = |outcome: &str| AttemptEnd { outcome: Some(outcome.into()), domain: Some("news.example".into()), ..Default::default() };

    let waiting = held(&repo, 1);
    let held_try = repo.start_attempt(waiting, "hold", now)?;
    repo.finish_attempt(held_try, &end("held"), now)?;
    let later = repo.start_attempt(waiting, "hold", now)?;
    repo.finish_attempt(later, &end("next_failed"), now)?;

    let done = queue(&repo, 2);
    assert_eq!(repo.lease_job("w", 180)?.expect("leased").id, done);
    let auto = repo.start_attempt(done, "hold", now)?;
    repo.finish_attempt(auto, &end("auto_delivered"), now)?;
    assert!(repo.finish(done, "w", JobStatus::Completed, None, None, None)?);
    let plain = queue(&repo, 3);

    let jobs: Vec<_> = [waiting, done, plain].iter().map(|id| repo.get_job(*id).unwrap().unwrap()).collect();
    let extras = repo.desk_extras(&jobs.iter().collect::<Vec<_>>())?;
    assert_eq!(extras.held.get(&waiting).map(|h| h.sha256.clone()), repo.waiting_held(waiting)?.map(|h| h.sha256));
    assert_eq!(extras.held_attempts.get(&waiting).map(|a| a.id), Some(held_try), "the latest held one, not the later failed one");
    assert_eq!(repo.latest_held_attempt(waiting)?.map(|a| a.id), Some(held_try));
    assert_eq!(extras.attempts.get(&done).map(|a| a.id), repo.latest_attempt(done)?.map(|a| a.id));
    assert_eq!(extras.attempts.get(&done).map(|a| a.id), Some(auto));
    assert!(!extras.held.contains_key(&plain) && !extras.attempts.contains_key(&plain) && !extras.held_attempts.contains_key(&done));
    assert!(repo.desk_extras(&[])?.held.is_empty(), "an empty page asks nothing");
    Ok(())
}
