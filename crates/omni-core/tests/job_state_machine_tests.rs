//! Job state machine: leasing, heartbeats, reaping, crash recovery and
//! idempotent enqueue (plan P1.1; defects D-01, D-02, D-10, D-20).

use std::time::Duration as StdDuration;

use chrono::Duration;
use omni_core::models::{Enqueued, JobStage, JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

fn queue(repo: &Repository, url: &str, slug: &str, journalist: &str) -> Enqueued {
    repo.enqueue(
        &NewJob::new(url, slug, journalist),
        DEFAULT_DEDUP_WINDOW_HOURS,
    )
    .unwrap()
}

// ---------------------------------------------------------------- leasing --

#[test]
fn leasing_is_exclusive_and_respects_priority() {
    let (_d, repo) = repo();

    queue(&repo, "https://example.gr/low", "1_MCR_LOW", "MCR");
    let high = repo
        .enqueue(
            &NewJob {
                priority: 100,
                ..NewJob::new("https://example.gr/high", "2_MCR_HIGH", "MCR")
            },
            DEFAULT_DEDUP_WINDOW_HOURS,
        )
        .unwrap()
        .job_id();

    // Highest priority first, regardless of insertion order.
    let first = repo.lease_job("hostA:100:0", 120).unwrap().expect("a job");
    assert_eq!(first.id, high, "priority was not honoured");
    assert_eq!(first.status, JobStatus::Running);
    assert_eq!(first.stage, JobStage::Extract);
    assert_eq!(first.attempts, 1);
    assert_eq!(first.lease_owner.as_deref(), Some("hostA:100:0"));

    // A second worker gets the other job, never the same one.
    let second = repo.lease_job("hostA:100:1", 120).unwrap().expect("a job");
    assert_ne!(second.id, first.id, "two workers leased the same job");

    // Nothing left.
    assert!(repo.lease_job("hostA:100:2", 120).unwrap().is_none());
}

#[test]
fn concurrent_workers_never_lease_the_same_job() {
    // Defect D-01's sibling: the lease itself must be atomic. 8 threads, 40
    // jobs, and every job must be leased exactly once.
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();

    for i in 1..=40 {
        queue(
            &repo,
            &format!("https://example.gr/v{i}"),
            &format!("{i}_MCR_ASSET"),
            "MCR",
        );
    }

    let mut handles = Vec::new();
    for w in 0..8 {
        let repo = repo.clone();
        handles.push(std::thread::spawn(move || {
            let mut mine = Vec::new();
            while let Some(job) = repo.lease_job(&format!("hostA:100:{w}"), 300).unwrap() {
                mine.push(job.id);
            }
            mine
        }));
    }

    let mut all: Vec<i64> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let leased = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(
        all.len(),
        leased,
        "a job was leased by more than one worker"
    );
    assert_eq!(leased, 40, "some jobs were never leased");
}

#[test]
fn a_job_is_not_leasable_before_its_backoff_expires() {
    let (_d, repo) = repo();
    let id = queue(&repo, "https://example.gr/v", "1_MCR_V", "MCR").job_id();

    let owner = "hostA:100:0";
    repo.lease_job(owner, 120).unwrap().unwrap();
    // A transient network failure: retry, but not immediately.
    assert!(repo
        .requeue_after(id, owner, Duration::minutes(5), "NETWORK", "Connection reset")
        .unwrap());

    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!(job.error_code.as_deref(), Some("NETWORK"));
    assert!(job.not_before.is_some());

    // The whole point: a worker must not immediately re-lease it and burn every
    // attempt inside a second.
    assert!(
        repo.lease_job(owner, 120).unwrap().is_none(),
        "a backed-off job was leased before not_before"
    );
}

// ------------------------------------------------------------- heartbeats --

#[test]
fn heartbeat_extends_our_lease_and_fails_once_we_lose_it() {
    let (_d, repo) = repo();
    let id = queue(&repo, "https://example.gr/v", "1_MCR_V", "MCR").job_id();

    let owner = "hostA:100:0";
    repo.lease_job(owner, 120).unwrap().unwrap();
    assert!(repo.heartbeat(id, owner, 120).unwrap());

    // Somebody else's heartbeat must never extend our lease.
    assert!(
        !repo.heartbeat(id, "hostB:200:0", 120).unwrap(),
        "a foreign owner extended a lease it does not hold"
    );

    // After the reaper takes it back, our heartbeat fails -- which is how the
    // worker learns to stop and not deliver a file for a job it no longer owns.
    repo.requeue_after(id, owner, Duration::seconds(0), "NETWORK", "x")
        .unwrap();
    assert!(
        !repo.heartbeat(id, owner, 120).unwrap(),
        "heartbeat succeeded on a job we no longer own"
    );
}

#[test]
fn finishing_a_job_we_no_longer_own_is_refused() {
    // The dangerous case: a worker whose lease was reaped mid-transcode must
    // not be able to mark the job COMPLETED after another worker picked it up.
    let (_d, repo) = repo();
    let id = queue(&repo, "https://example.gr/v", "1_MCR_V", "MCR").job_id();

    let first = "hostA:100:0";
    repo.lease_job(first, 120).unwrap().unwrap();
    repo.requeue_after(id, first, Duration::seconds(0), "NETWORK", "x")
        .unwrap();

    let second = "hostA:100:1";
    repo.lease_job(second, 120).unwrap().unwrap();

    assert!(
        !repo
            .finish(id, first, JobStatus::Completed, None, None, Some("stale.mxf"))
            .unwrap(),
        "a stale worker marked someone else's job completed"
    );

    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Running);
    assert_eq!(job.file_path, None);

    // The rightful owner can finish it.
    assert!(repo
        .finish(id, second, JobStatus::Completed, None, None, Some("1_MCR_V.mxf"))
        .unwrap());
    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.stage, JobStage::Done);
    assert!(job.completed_at.is_some());
    assert!(job.delivered_at.is_some());
    assert!(job.lease_owner.is_none(), "lease was not released");
}

// ------------------------------------------------- reaping and recovery --

#[test]
fn an_expired_lease_is_requeued_while_attempts_remain() {
    let (_d, repo) = repo();
    let id = queue(&repo, "https://example.gr/v", "1_MCR_V", "MCR").job_id();

    // A lease that has already expired: the worker died without releasing it.
    repo.lease_job("hostA:100:0", -1).unwrap().unwrap();

    let (requeued, review) = repo.reap_expired_leases().unwrap();
    assert_eq!((requeued, review), (1, 0));

    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!(job.stage, JobStage::Queued);
    assert!(job.lease_owner.is_none());
    assert_eq!(job.attempts, 1, "the failed attempt must still count");

    // It is immediately leasable again.
    assert!(repo.lease_job("hostA:100:1", 120).unwrap().is_some());
}

#[test]
fn a_job_that_exhausts_its_attempts_goes_to_review_not_an_infinite_retry() {
    let (_d, repo) = repo();
    let id = queue(&repo, "https://example.gr/v", "1_MCR_V", "MCR").job_id();

    // max_attempts defaults to 3.
    for attempt in 1..=3 {
        repo.lease_job(&format!("hostA:100:{attempt}"), -1)
            .unwrap()
            .expect("job should still be leasable");
        repo.reap_expired_leases().unwrap();
    }

    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!(
        job.status,
        JobStatus::RequiresReview,
        "an unfinishable job must reach a human, not retry forever"
    );
    assert_eq!(job.error_code.as_deref(), Some("LEASE_EXPIRED"));
    assert_eq!(job.attempts, 3);
    assert!(repo.lease_job("hostA:100:9", 120).unwrap().is_none());
}

#[test]
fn restart_recovers_this_hosts_orphans_and_leaves_other_hosts_alone() {
    // Defect D-02: kill the daemon mid-transcode and the job stays RUNNING
    // forever -- no owner, and nobody watching its lease.
    let (_d, repo) = repo();
    let mine = queue(&repo, "https://example.gr/a", "1_MCR_A", "MCR").job_id();
    let theirs = queue(&repo, "https://example.gr/b", "2_MCR_B", "MCR").job_id();

    repo.lease_job("mcr-pc:4100:0", 3600).unwrap().unwrap();
    repo.lease_job("other-pc:900:0", 3600).unwrap().unwrap();

    let recovered = repo.recover_on_startup("mcr-pc").unwrap();
    assert_eq!(recovered, 1);

    let a = repo.get_job(mine).unwrap().unwrap();
    assert_eq!(a.status, JobStatus::Pending);
    assert!(a.lease_owner.is_none());

    // A second daemon sharing the database must keep its work.
    let b = repo.get_job(theirs).unwrap().unwrap();
    assert_eq!(b.status, JobStatus::Running);
    assert_eq!(b.lease_owner.as_deref(), Some("other-pc:900:0"));

    // The recovery is visible to the operator in the job timeline.
    let events = repo.get_job_events(mine, 50).unwrap();
    assert!(
        events.iter().any(|e| e.message.contains("Recovered after daemon restart")),
        "recovery was not recorded in the job timeline: {events:?}"
    );
}

// ------------------------------------------------------ idempotent enqueue --

#[test]
fn the_same_link_is_not_queued_twice_while_active() {
    let (_d, repo) = repo();

    let first = queue(
        &repo,
        "https://www.youtube.com/watch?v=mOMyiwJCX6I",
        "1_PAPADAKI_KNICKS",
        "PAPADAKI",
    );
    assert!(first.is_new());

    // The same video, forwarded by a colleague in a different URL shape.
    // Without normalization this would deliver two identical MXFs.
    let second = queue(
        &repo,
        "https://youtu.be/mOMyiwJCX6I?si=abc123",
        "2_PAPADAKI_KNICKS",
        "PAPADAKI",
    );
    assert_eq!(
        second,
        Enqueued::DuplicateActive {
            existing_id: first.job_id()
        },
        "a re-sent link created a second job"
    );

    // ...and still a duplicate once it is running.
    repo.lease_job("hostA:100:0", 120).unwrap().unwrap();
    let third = queue(
        &repo,
        "https://m.youtube.com/watch?v=mOMyiwJCX6I&feature=share",
        "3_PAPADAKI_KNICKS",
        "PAPADAKI",
    );
    assert!(matches!(third, Enqueued::DuplicateActive { .. }));
}

#[test]
fn a_different_journalist_may_queue_a_completed_link_again() {
    // Two journalists genuinely need their own slug in their own folder.
    let (_d, repo) = repo();
    let owner = "hostA:100:0";

    let first = queue(&repo, "https://example.gr/story", "1_PAPADAKI_X", "PAPADAKI").job_id();
    repo.lease_job(owner, 120).unwrap().unwrap();
    repo.finish(first, owner, JobStatus::Completed, None, None, Some("out.mxf"))
        .unwrap();

    // Same journalist, inside the window: a duplicate.
    let same = queue(&repo, "https://example.gr/story", "9_PAPADAKI_X", "PAPADAKI");
    assert_eq!(same, Enqueued::DuplicateRecent { existing_id: first });

    // Different journalist: a real new job.
    let other = queue(&repo, "https://example.gr/story", "1_NIKOLAOU_X", "NIKOLAOU");
    assert!(
        other.is_new(),
        "a second journalist was blocked from queuing the same story"
    );
}

#[test]
fn a_completed_link_is_queueable_again_outside_the_dedup_window() {
    let (_d, repo) = repo();
    let owner = "hostA:100:0";
    let first = queue(&repo, "https://example.gr/story", "1_PAPADAKI_X", "PAPADAKI").job_id();
    repo.lease_job(owner, 120).unwrap().unwrap();
    repo.finish(first, owner, JobStatus::Completed, None, None, None)
        .unwrap();

    // A zero-hour window means "nothing counts as recent".
    let again = repo
        .enqueue(
            &NewJob::new("https://example.gr/story", "2_PAPADAKI_X", "PAPADAKI"),
            0,
        )
        .unwrap();
    assert!(
        again.is_new(),
        "a story could not be re-ingested after the dedup window"
    );
}

#[test]
fn a_discarded_job_does_not_block_re_queuing_the_same_link() {
    // The old UNIQUE(url) made this impossible: once a link was in the table,
    // re-sending it after a failure produced a raw constraint error (D-10).
    let (_d, repo) = repo();
    let owner = "hostA:100:0";
    let first = queue(&repo, "https://example.gr/story", "1_MCR_X", "MCR").job_id();
    repo.lease_job(owner, 120).unwrap().unwrap();
    repo.finish(
        first,
        owner,
        JobStatus::Failed,
        Some("NO_STREAM_FOUND"),
        Some("nothing found"),
        None,
    )
    .unwrap();

    let retry = queue(&repo, "https://example.gr/story", "1_MCR_X", "MCR");
    assert!(retry.is_new(), "a failed link could not be re-queued: {retry:?}");
}

#[test]
fn enqueue_applies_the_journalists_default_priority() {
    // Defect D-20: journalists.default_priority existed but was never read, so
    // the newsroom's "this reporter is always urgent" setting did nothing.
    let (_d, repo) = repo();
    repo.save_journalist("NIKOLAOU", "G. Nikolaou", &["g@example.gr".to_string()], 50)
        .unwrap();

    let id = queue(&repo, "https://example.gr/v", "1_NIKOLAOU_V", "NIKOLAOU").job_id();
    assert_eq!(repo.get_job(id).unwrap().unwrap().priority, 50);

    // An explicitly higher priority still wins -- the default is a floor.
    let id = repo
        .enqueue(
            &NewJob {
                priority: 100,
                ..NewJob::new("https://example.gr/w", "2_NIKOLAOU_W", "NIKOLAOU")
            },
            DEFAULT_DEDUP_WINDOW_HOURS,
        )
        .unwrap()
        .job_id();
    assert_eq!(repo.get_job(id).unwrap().unwrap().priority, 100);
}

#[test]
fn the_job_timeline_records_what_happened() {
    let (_d, repo) = repo();
    let id = queue(&repo, "https://example.gr/v", "1_MCR_V", "MCR").job_id();
    let owner = "hostA:100:0";

    repo.lease_job(owner, 120).unwrap().unwrap();
    assert!(repo.set_stage(id, owner, JobStage::Download).unwrap());
    assert!(repo.set_stage(id, owner, JobStage::Transcode).unwrap());
    repo.record_event(id, "WARN", Some(JobStage::Transcode), "Source has no audio stream")
        .unwrap();
    repo.finish(id, owner, JobStatus::Completed, None, None, Some("out.mxf"))
        .unwrap();

    let events = repo.get_job_events(id, 100).unwrap();
    let messages: Vec<&str> = events.iter().map(|e| e.message.as_str()).collect();
    assert!(messages.iter().any(|m| m.contains("Queued as 1_MCR_V")), "{messages:?}");
    assert!(messages.iter().any(|m| m.contains("Leased by")), "{messages:?}");
    assert!(messages.iter().any(|m| m.contains("Stage DOWNLOAD")), "{messages:?}");
    assert!(messages.iter().any(|m| m.contains("no audio stream")), "{messages:?}");
    assert!(messages.iter().any(|m| m.contains("Finished as COMPLETED")), "{messages:?}");

    // Events carry real timestamps in order.
    assert!(events.iter().all(|e| e.at.is_some()));
    assert!(events.windows(2).all(|w| w[0].id < w[1].id));
}

#[test]
fn stage_changes_from_a_foreign_owner_are_refused() {
    let (_d, repo) = repo();
    let id = queue(&repo, "https://example.gr/v", "1_MCR_V", "MCR").job_id();
    repo.lease_job("hostA:100:0", 120).unwrap().unwrap();
    assert!(
        !repo.set_stage(id, "hostB:200:0", JobStage::Deliver).unwrap(),
        "a foreign worker advanced someone else's job"
    );
}

#[test]
fn leasing_holds_only_briefly_under_contention() {
    // The lease transaction is IMMEDIATE, so it serialises writers. Guard
    // against a future change that holds it across expensive work: 40 leases
    // across 4 threads should be quick, not minutes.
    let (_d, repo) = repo();
    for i in 1..=40 {
        queue(
            &repo,
            &format!("https://example.gr/v{i}"),
            &format!("{i}_MCR_A"),
            "MCR",
        );
    }
    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for w in 0..4 {
        let repo = repo.clone();
        handles.push(std::thread::spawn(move || {
            while repo.lease_job(&format!("h:1:{w}"), 300).unwrap().is_some() {}
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert!(
        start.elapsed() < StdDuration::from_secs(20),
        "leasing 40 jobs took {:?}; the lease transaction is holding a lock too long",
        start.elapsed()
    );
}

#[test]
fn only_the_lease_holder_corrects_a_jobs_link_and_dedup_follows_it() {
    // P3.12: a job queued as "…/arthro/-ΒΙΝΤΕΟ" is corrected while it runs;
    // the desk then opens, and dedups, the address that works.
    let (_d, repo) = repo();
    let id = queue(&repo, "https://www.example.gr/kosmos/arthro/-ΒΙΝΤΕΟ", "1_MCR_ARTHRO", "MCR").job_id();
    let job = repo.lease_job("hostA:1:1", 120).unwrap().expect("a job");
    assert_eq!(job.id, id);

    assert!(!repo.set_leased_job_url(id, "hostA:1:2", "https://www.example.gr/kosmos/arthro/").unwrap(), "not the lease holder");
    assert!(repo.set_leased_job_url(id, "hostA:1:1", "https://www.example.gr/kosmos/arthro/").unwrap());
    assert_eq!(repo.get_job(id).unwrap().unwrap().url, "https://www.example.gr/kosmos/arthro/");
    assert_eq!(
        queue(&repo, "https://www.example.gr/kosmos/arthro/", "2_MCR_ARTHRO", "MCR"),
        Enqueued::DuplicateActive { existing_id: id },
        "the corrected address is the one dedup knows"
    );
}

#[test]
fn a_job_can_be_renamed_until_its_file_is_being_made() {
    // P7.12: MCR fixes an uncertain keyword before delivery, never after
    // the rewrap has named the clip.
    let (_d, repo) = repo();
    let id = queue(&repo, "https://example.gr/rename", "1_MCR_TREILER", "MCR").job_id();
    let (old, new) = repo.rename_job_keyword(id, "DUNE").unwrap().expect("a waiting job");
    assert_eq!((old.as_str(), new.as_str()), ("1_MCR_TREILER", "1_MCR_DUNE"));
    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!((job.slug.as_str(), job.keyword.as_str()), ("1_MCR_DUNE", "DUNE"));

    repo.lease_job("hostA:1:1", 120).unwrap().expect("leased");
    repo.set_stage(id, "hostA:1:1", JobStage::Transcode).unwrap();
    assert!(repo.rename_job_keyword(id, "DUNEPART").unwrap().is_some(), "still before the rewrap");
    repo.set_stage(id, "hostA:1:1", JobStage::Rewrap).unwrap();
    assert!(repo.rename_job_keyword(id, "LATE").unwrap().is_none(), "the clip is being named");
    assert_eq!(repo.get_job(id).unwrap().unwrap().slug, "1_MCR_DUNEPART");

    repo.finish(id, "hostA:1:1", JobStatus::Completed, None, None, Some("x.mxf")).unwrap();
    assert!(repo.rename_job_keyword(id, "AFTER").unwrap().is_none(), "delivered files are never renamed");
    assert!(repo.rename_job_keyword(99_999, "GONE").unwrap().is_none());
}

// ------------------------------------------------------- marked by hand --

#[test]
fn a_job_can_be_marked_done_only_from_review_states() {
    let (_d, repo) = repo();
    let owner = "hostA:100:0";

    // The three states that wait for a person.
    for (n, status) in [JobStatus::RequiresReview, JobStatus::ManualDownload, JobStatus::Failed]
        .into_iter()
        .enumerate()
    {
        let url = format!("https://example.gr/review-{n}");
        let id = queue(&repo, &url, &format!("{n}_NIKOLAOU_R"), "NIKOLAOU").job_id();
        repo.lease_job(owner, 120).unwrap().unwrap();
        assert!(repo
            .finish(id, owner, status, Some("E_TEST"), Some("why"), None)
            .unwrap());
        assert_eq!(repo.get_job(id).unwrap().unwrap().status, status);

        assert!(repo.mark_completed_manually(id).unwrap(), "{status:?}");
        let job = repo.get_job(id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::CompletedManual);
        assert!(job.completed_at.is_some());
        assert!(job.delivered_at.is_some(), "the Email tab's «Παραδόθηκε …» time");
        assert_eq!(job.stage, JobStage::Done);
        assert_eq!(job.error_code.as_deref(), Some("E_TEST"));
        assert!(job.file_path.is_none());

        // Marked once; a second press changes nothing.
        assert!(!repo.mark_completed_manually(id).unwrap());

        // Dedup treats it as delivered.
        let again = queue(&repo, &url, &format!("9{n}_NIKOLAOU_R"), "NIKOLAOU");
        assert_eq!(again, Enqueued::DuplicateRecent { existing_id: id });
    }

    // Everything else is refused and left alone.
    let pending = queue(&repo, "https://example.gr/pending", "7_NIKOLAOU_P", "NIKOLAOU").job_id();
    assert!(!repo.mark_completed_manually(pending).unwrap());
    assert_eq!(repo.get_job(pending).unwrap().unwrap().status, JobStatus::Pending);

    repo.lease_job(owner, 120).unwrap().unwrap();
    assert!(!repo.mark_completed_manually(pending).unwrap());
    let running = repo.get_job(pending).unwrap().unwrap();
    assert_eq!(running.status, JobStatus::Running);
    assert_eq!(running.lease_owner.as_deref(), Some(owner));

    repo.finish(pending, owner, JobStatus::Completed, None, None, Some("out.mxf"))
        .unwrap();
    assert!(!repo.mark_completed_manually(pending).unwrap());
    let done = repo.get_job(pending).unwrap().unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.file_path.as_deref(), Some("out.mxf"));
}
