//! The autonomy report (plan P11.0): which jobs a person touched and why, one
//! outcome row per ending, and the report built from those rows. Real SQLite
//! files; every name and address is invented.

use anyhow::Result;
use chrono::{Duration, Utc};
use rusqlite::{params, Connection};
use tempfile::TempDir;

use omni_core::models::{JobStage, JobStatus, NewJob, UserRole};
use omni_core::recipe_score::{InsertOutcome, NewRecipe, Origin, ScopeKind};
use omni_core::repository::{
    AttemptEnd, CancelOutcome, DeliverySettled, OwnCancelOutcome, Repository, WrongVideoOutcome, OUTCOME_KEEP_DAYS,
};
use omni_core::timestamps;

const OWNER: &str = "testhost:1:0";

fn sha() -> String {
    "ab".repeat(32)
}

struct Rig {
    _dir: TempDir,
    db: std::path::PathBuf,
    repo: Repository,
    n: u32,
}

fn rig() -> Rig {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("omni.db");
    let repo = Repository::new(&db).unwrap();
    Rig { _dir: dir, db, repo, n: 0 }
}

/// (outcome, touched, touch_reason, first_error_code, site, from_mail) of one outcome row.
type OutcomeRow = (String, bool, Option<String>, Option<String>, Option<String>, bool);

impl Rig {
    fn raw(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    /// A new PENDING job on its own site.
    fn job(&mut self) -> i64 {
        self.n += 1;
        let n = self.n;
        self.enqueue(NewJob::new(format!("https://www.site{n}.example/news/story-{n}"), format!("{n}_TESTOPOULOS_TEST"), "Testopoulos"))
    }

    fn enqueue(&self, job: NewJob) -> i64 {
        self.repo.enqueue(&job, 0).unwrap().job_id()
    }

    fn lease(&self, id: i64) {
        assert!(self.repo.lease_job_by_id(id, OWNER, 180).unwrap(), "job {id} leases");
    }

    /// Leased and finished in `status` with `code`.
    fn ended(&mut self, status: JobStatus, code: Option<&str>) -> i64 {
        let id = self.job();
        self.lease(id);
        assert!(self.repo.finish(id, OWNER, status, code, code, None).unwrap());
        id
    }

    fn touch(&self, id: i64) -> (bool, Option<String>, Option<String>) {
        let j = self.repo.get_job(id).unwrap().unwrap();
        (j.human_touched, j.touch_reason, j.first_error_code)
    }

    fn outcomes(&self, id: i64) -> Vec<OutcomeRow> {
        self.raw()
            .prepare(
                "SELECT outcome, touched, touch_reason, first_error_code, site, from_mail FROM job_outcomes
                 WHERE job_id = ? ORDER BY id",
            )
            .unwrap()
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn all_outcomes(&self) -> i64 {
        self.raw().query_row("SELECT COUNT(*) FROM job_outcomes", [], |r| r.get(0)).unwrap()
    }

    /// Every write to `job_outcomes` fails from now on, as a damaged table would.
    fn break_outcomes(&self) {
        self.raw()
            .execute_batch(
                "CREATE TRIGGER no_outcome_insert BEFORE INSERT ON job_outcomes BEGIN SELECT RAISE(ABORT, 'injected'); END;
                 CREATE TRIGGER no_outcome_update BEFORE UPDATE ON job_outcomes BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
    }

    /// A job leased by OWNER at DELIVER.
    fn at_deliver(&mut self) -> i64 {
        let id = self.job();
        self.lease(id);
        assert!(self.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
        id
    }

    /// A file of `bytes` bytes in the test's watchfolder.
    fn delivered_file(&self, name: &str, bytes: usize) -> String {
        let dir = self._dir.path().join("watchfolder");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, vec![7u8; bytes]).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Leased, finished as COMPLETED; the site its outcome row names.
    fn site_of(&self, url: &str) -> Option<String> {
        let id = self.enqueue(NewJob::new(url, "1_TESTOPOULOS_SITE", "Testopoulos"));
        self.lease(id);
        assert!(self.repo.finish(id, OWNER, JobStatus::Completed, None, None, None).unwrap());
        let rows = self.outcomes(id);
        assert_eq!(rows.len(), 1);
        rows[0].4.clone()
    }

    /// A job parked AWAITING_APPROVAL by a repair attempt that holds `urls[0]`.
    fn held(&mut self, urls: &[&str]) -> i64 {
        let id = self.job();
        self.lease(id);
        let at = self.repo.start_attempt(id, "hold", Utc::now()).unwrap();
        let list: Vec<serde_json::Value> = urls.iter().map(|u| serde_json::json!({ "url": u, "origin": "recipe:x" })).collect();
        let end = AttemptEnd {
            rung: Some("recipes".into()),
            outcome: Some("held".into()),
            domain: Some("news.example".into()),
            candidates_json: Some(serde_json::Value::Array(list).to_string()),
            held_url: Some(urls[0].to_string()),
            next_index: 0,
            ..Default::default()
        };
        self.repo.finish_attempt(at, &end, Utc::now()).unwrap();
        assert!(self.repo.hold_output(id, OWNER, "temp/held/x/clip.mxf", 10, &sha(), Some(9.0), Utc::now() + Duration::hours(24)).unwrap());
        assert!(self.repo.finish(id, OWNER, JobStatus::AwaitingApproval, None, None, None).unwrap());
        id
    }

    /// A COMPLETED job a trusted recipe delivered without approval.
    fn auto_delivered(&mut self) -> i64 {
        let new = NewRecipe {
            scope_kind: ScopeKind::Site,
            scope_key: "news.example".into(),
            family: None,
            recipe_json: "{}".into(),
            hash: format!("hash-{}", self.n),
            steps_hash: format!("steps-{}", self.n),
            dsl_version: 1,
            origin: Origin::Admin,
        };
        let rid = match self.repo.insert_recipe(&new, Utc::now()).unwrap() {
            InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) => id,
            other => panic!("recipe not stored: {other:?}"),
        };
        let id = self.job();
        self.lease(id);
        let at = self.repo.start_attempt(id, "hold", Utc::now()).unwrap();
        let end = AttemptEnd {
            rung: Some("recipes".into()),
            outcome: Some("auto_delivered".into()),
            recipe_id: Some(rid),
            recipe_hash: Some(new.hash.clone()),
            domain: Some("news.example".into()),
            held_url: Some("https://cdn.news.example/v/a.mp4".into()),
            ..Default::default()
        };
        self.repo.finish_attempt(at, &end, Utc::now()).unwrap();
        assert!(self.repo.finish(id, OWNER, JobStatus::Completed, None, None, Some("watch/clip.mxf")).unwrap());
        id
    }
}

fn reason(r: &str) -> (bool, Option<String>) {
    (true, Some(r.to_string()))
}

// ---------------------------------------------------------------- touches

#[test]
fn each_operator_action_marks_the_job_touched_with_its_reason() -> Result<()> {
    let mut r = rig();
    let t = |r: &Rig, id: i64| {
        let (touched, why, _) = r.touch(id);
        (touched, why)
    };

    let retry = r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    assert!(r.repo.retry_job(retry, None)?);
    assert_eq!(t(&r, retry), reason("retry"));

    let over = r.ended(JobStatus::RequiresReview, Some("VIDEO_UNRESOLVED"));
    assert!(r.repo.retry_job(over, Some("https://www.other.example/v/2"))?);
    assert_eq!(t(&r, over), reason("override"));

    let rename = r.job();
    assert!(r.repo.rename_job_keyword(rename, "LIMANI")?.is_some());
    assert_eq!(t(&r, rename), reason("rename"));

    let redo = r.ended(JobStatus::Completed, None);
    assert!(r.repo.redownload_job(redo)?);
    assert_eq!(t(&r, redo), reason("redownload"));

    let manual = r.ended(JobStatus::ManualDownload, Some("MANUAL_DOWNLOAD"));
    assert!(r.repo.mark_completed_manually(manual)?);
    assert_eq!(t(&r, manual), reason("manual_done"));

    let approve = r.held(&["https://cdn.news.example/v/a.mp4"]);
    assert!(r.repo.approve_held(approve, &sha(), "desk")?);
    assert_eq!(t(&r, approve), reason("approve"));

    let reject = r.held(&["https://cdn.news.example/v/b.mp4"]);
    assert!(r.repo.reject_held(reject, "not_usable", "no", "desk")?);
    assert_eq!(t(&r, reject), reason("reject"));

    let next = r.held(&["https://cdn.news.example/v/c.mp4", "https://cdn.news.example/v/d.mp4"]);
    assert_eq!(r.repo.next_candidate(next, &sha(), "desk", Utc::now())?, Some(1));
    assert_eq!(t(&r, next), reason("next"));

    let wrong = r.auto_delivered();
    assert_eq!(t(&r, wrong), (false, None), "an automatic delivery is untouched");
    assert_eq!(r.repo.wrong_video_after_auto_delivery(wrong, "desk", Utc::now())?, WrongVideoOutcome::Recorded);
    assert_eq!(t(&r, wrong), reason("wrong_video"));

    let cancel = r.job();
    assert_eq!(r.repo.cancel_job(cancel)?, CancelOutcome::Cancelled);
    assert_eq!(t(&r, cancel), reason("cancel"));

    let user = r.repo.create_user("reporter@example.invalid", "correct horse battery", UserRole::User, "Test Reporter", None)?;
    let own = r.enqueue(NewJob { submitted_by_user_id: Some(user), ..NewJob::new("https://www.own.example/v/1", "9_TESTOPOULOS_OWN", "Testopoulos") });
    assert_eq!(r.repo.cancel_own_pending(own, user)?, OwnCancelOutcome::Cancelled);
    assert_eq!(t(&r, own), reason("cancel"));

    let now = r.job();
    r.lease(now);
    assert!(r.repo.requeue_after(now, OWNER, Duration::seconds(600), "NETWORK", "timed out")?);
    assert_eq!(t(&r, now), (false, None), "an automatic retry is no touch");
    assert!(r.repo.run_now(now)?);
    assert_eq!(t(&r, now), reason("now"));
    Ok(())
}

#[test]
fn a_job_mcr_queued_by_hand_starts_touched_and_a_duplicate_writes_nothing() -> Result<()> {
    let r = rig();
    let mail = r.enqueue(NewJob {
        email_message_id: Some("<invented-4@mail.example>".into()),
        ..NewJob::new("https://www.mailsite.example/v/4", "1_TESTOPOULOS_MAIL", "Testopoulos")
    });
    assert_eq!(r.touch(mail), (false, None, None), "a job the mail made is untouched");
    let by_hand = r.enqueue(NewJob {
        queued_by_operator: true,
        email_message_id: Some("<invented-4@mail.example>".into()),
        ..NewJob::new("https://www.mailsite.example/v/5", "2_TESTOPOULOS_MAIL", "Testopoulos")
    });
    assert_eq!(r.touch(by_hand), (true, Some("queued".into()), None));
    // The same link again while the first is waiting: a duplicate, nothing written.
    let again = r.repo.enqueue(
        &NewJob { queued_by_operator: true, ..NewJob::new("https://www.mailsite.example/v/4", "3_TESTOPOULOS_MAIL", "Testopoulos") },
        0,
    )?;
    assert_eq!(again.job_id(), mail);
    assert_eq!(r.touch(mail), (false, None, None));
    Ok(())
}

#[test]
fn a_refused_action_does_not_touch_the_job() -> Result<()> {
    let mut r = rig();
    let running = r.job();
    r.lease(running);
    assert!(!r.repo.retry_job(running, None)?, "a running job is not retried");
    assert!(!r.repo.mark_completed_manually(running)?);
    assert!(!r.repo.redownload_job(running)?);
    assert!(!r.repo.run_now(running)?);
    assert!(!r.touch(running).0);
    let done = r.ended(JobStatus::Completed, None);
    assert_eq!(r.repo.cancel_job(done)?, CancelOutcome::NotCancellable);
    assert!(r.repo.rename_job_keyword(done, "LATE")?.is_none());
    assert!(!r.touch(done).0);
    Ok(())
}

#[test]
fn a_second_action_keeps_the_first_reason() -> Result<()> {
    let mut r = rig();
    let id = r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    assert!(r.repo.rename_job_keyword(id, "LIMANI")?.is_some());
    assert!(r.repo.retry_job(id, None)?);
    assert_eq!(r.repo.cancel_job(id)?, CancelOutcome::Cancelled);
    assert_eq!(r.touch(id), (true, Some("rename".into()), Some("NETWORK".into())));
    Ok(())
}

#[test]
fn an_automatic_retry_then_a_delivery_is_untouched_and_keeps_its_first_error() -> Result<()> {
    let mut r = rig();
    let id = r.job();
    r.lease(id);
    assert!(r.repo.heartbeat(id, OWNER, 180)?);
    assert!(r.repo.requeue_retry(id, OWNER, Duration::seconds(0), "NETWORK", "reset by peer", 4)?);
    r.raw().execute("UPDATE queue SET not_before = NULL WHERE id = ?", params![id])?;
    r.lease(id);
    assert!(r.repo.requeue_retry(id, OWNER, Duration::seconds(0), "HTTP_429", "rate limited", 4)?);
    r.raw().execute("UPDATE queue SET not_before = NULL WHERE id = ?", params![id])?;
    r.lease(id);
    assert!(r.repo.finish(id, OWNER, JobStatus::Completed, None, None, Some("watch/clip.mxf"))?);

    let job = r.repo.get_job(id)?.unwrap();
    assert_eq!(job.error_code, None, "a delivery clears the current error");
    assert_eq!(r.touch(id), (false, None, Some("NETWORK".into())), "the first error stays, nobody touched it");
    let rows = r.outcomes(id);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].0.as_str(), rows[0].1, rows[0].3.as_deref()), ("delivered", false, Some("NETWORK")));
    Ok(())
}

#[test]
fn the_reaper_recovery_and_the_watchfolder_wait_never_touch() -> Result<()> {
    let mut r = rig();
    // The reaper: an expired lease with attempts left is requeued, one without goes to review.
    let reaped = r.job();
    assert!(r.repo.lease_job_by_id(reaped, OWNER, -60)?);
    assert_eq!(r.repo.reap_expired_leases()?, (1, 0));
    assert_eq!(r.touch(reaped), (false, None, None));
    r.raw().execute("UPDATE queue SET attempts = max_attempts - 1 WHERE id = ?", params![reaped])?;
    assert!(r.repo.lease_job_by_id(reaped, OWNER, -60)?);
    assert_eq!(r.repo.reap_expired_leases()?, (0, 1));
    assert_eq!(r.touch(reaped), (false, None, Some("LEASE_EXPIRED".into())));

    // Start-up recovery of a run that was not delivered.
    let recovered = r.job();
    r.lease(recovered);
    assert_eq!(r.repo.recover_on_startup("testhost")?, 1);
    assert_eq!(r.touch(recovered), (false, None, None));

    // The watchfolder wait: parked, requeued while the share is down, delivered.
    let waiting = r.job();
    r.lease(waiting);
    assert!(r.repo.park_for_delivery(waiting, OWNER, "temp/jobs/x/ready.mxf", 10, &sha(), 9.0)?);
    assert!(r.repo.requeue_waiting_delivery(waiting, OWNER, Duration::seconds(0), "the watchfolder does not answer")?);
    assert_eq!(r.touch(waiting), (false, None, Some("DELIVERY_FAILED".into())));
    r.lease(waiting);
    assert!(r.repo.set_delivered_file(waiting, OWNER, "watch/clip.mxf", 9.0)?);
    assert!(r.repo.finish(waiting, OWNER, JobStatus::Completed, None, None, None)?);
    let rows = r.outcomes(waiting);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].0.as_str(), rows[0].1), ("delivered", false));
    Ok(())
}

// ---------------------------------------------------------------- outcome rows

#[test]
fn every_completion_path_writes_one_delivered_row() -> Result<()> {
    let mut r = rig();
    // The worker's finish.
    let plain = r.ended(JobStatus::Completed, None);
    assert_eq!(r.outcomes(plain).len(), 1);
    assert_eq!(r.outcomes(plain)[0].0, "delivered");

    // An approved held video's delivery.
    let held = r.held(&["https://cdn.news.example/v/a.mp4"]);
    assert!(r.repo.approve_held(held, &sha(), "desk")?);
    r.lease(held);
    assert!(r.repo.record_held_delivery(held, OWNER, "watch/a.mxf", 2.0)?);
    assert!(r.repo.finish_held_delivery(held, OWNER)?);
    let rows = r.outcomes(held);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].0.as_str(), rows[0].1, rows[0].2.as_deref()), ("delivered", true, Some("approve")));

    // Start-up recovery of a run that had reached ARCHIVE (already in the watchfolder).
    let archived = r.job();
    r.lease(archived);
    assert!(r.repo.set_stage(archived, OWNER, JobStage::Archive)?);
    assert_eq!(r.outcomes(archived).len(), 0, "not ended yet");
    r.repo.recover_on_startup("testhost")?;
    assert_eq!(r.repo.get_job(archived)?.unwrap().status, JobStatus::Completed);
    assert_eq!(r.outcomes(archived).len(), 1);

    // The reaper's recovery of the same case.
    let reaped = r.job();
    assert!(r.repo.lease_job_by_id(reaped, OWNER, -60)?);
    assert!(r.repo.set_stage(reaped, OWNER, JobStage::Archive)?);
    r.repo.reap_expired_leases()?;
    assert_eq!(r.repo.get_job(reaped)?.unwrap().status, JobStatus::Completed);
    assert_eq!(r.outcomes(reaped).len(), 1);
    Ok(())
}

#[test]
fn completing_twice_cannot_write_two_rows_for_one_ending() -> Result<()> {
    let mut r = rig();
    let id = r.job();
    r.lease(id);
    assert!(r.repo.set_stage(id, OWNER, JobStage::Archive)?);
    assert!(r.repo.finish(id, OWNER, JobStatus::Completed, None, None, None)?);
    // Every other completion path finds it no longer running.
    assert!(!r.repo.finish(id, OWNER, JobStatus::Completed, None, None, None)?);
    assert!(!r.repo.finish_held_delivery(id, OWNER)?);
    assert_eq!(r.repo.recover_on_startup("testhost")?, 0);
    assert_eq!(r.repo.reap_expired_leases()?, (0, 0));
    assert!(!r.repo.mark_completed_manually(id)?);
    assert_eq!(r.repo.cancel_job(id)?, CancelOutcome::NotCancellable);
    assert_eq!(r.outcomes(id).len(), 1);

    let cancelled = r.job();
    assert_eq!(r.repo.cancel_job(cancelled)?, CancelOutcome::Cancelled);
    assert_eq!(r.repo.cancel_job(cancelled)?, CancelOutcome::NotCancellable);
    assert_eq!(r.outcomes(cancelled).len(), 1, "a second cancel is refused and writes nothing");

    let review = r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    assert_eq!(r.repo.clear_failed_jobs()?, 1);
    assert_eq!(r.repo.clear_failed_jobs()?, 0);
    assert_eq!(r.outcomes(review).len(), 1, "clearing again finds nothing to count");
    Ok(())
}

#[test]
fn a_wrong_video_after_an_automatic_delivery_counts_against_the_gate_figure() -> Result<()> {
    let mut r = rig();
    let other = r.ended(JobStatus::Completed, None);
    let wrong = r.auto_delivered();
    let since = Utc::now() - Duration::days(1);
    let before = r.repo.autonomy_report(since)?;
    assert_eq!(before.untouched_rate, Some(1.0), "two untouched deliveries");
    let row_id = |r: &Rig| -> i64 {
        r.raw().query_row("SELECT id FROM job_outcomes WHERE job_id = ?", params![wrong], |x| x.get(0)).unwrap()
    };
    let id_before = row_id(&r);

    assert_eq!(r.repo.wrong_video_after_auto_delivery(wrong, "desk", Utc::now())?, WrongVideoOutcome::Recorded);
    let rows = r.outcomes(wrong);
    assert_eq!(rows.len(), 1, "the same row, no new one");
    assert_eq!(row_id(&r), id_before);
    assert_eq!((rows[0].0.as_str(), rows[0].1, rows[0].2.as_deref()), ("delivered", true, Some("wrong_video")));
    assert!(!r.outcomes(other)[0].1, "another job's row is left alone");
    let after = r.repo.autonomy_report(since)?;
    assert_eq!(after.untouched_rate, Some(0.5));
    assert_eq!(after.totals.delivered_untouched, 1);
    assert!(after.by_touch_reason.iter().any(|c| c.key == "wrong_video" && c.count == 1), "{:?}", after.by_touch_reason);

    // A job with no delivered row (delivered before the report existed): nothing to change.
    let old = r.auto_delivered();
    r.raw().execute("DELETE FROM job_outcomes WHERE job_id = ?", params![old])?;
    let snapshot = r.all_outcomes();
    let touched_rows: i64 = r.raw().query_row("SELECT COUNT(*) FROM job_outcomes WHERE touched = 1", [], |x| x.get(0))?;
    assert_eq!(r.repo.wrong_video_after_auto_delivery(old, "desk", Utc::now())?, WrongVideoOutcome::Recorded);
    assert_eq!(r.all_outcomes(), snapshot);
    let touched_after: i64 = r.raw().query_row("SELECT COUNT(*) FROM job_outcomes WHERE touched = 1", [], |x| x.get(0))?;
    assert_eq!(touched_after, touched_rows, "no other row is rewritten");
    Ok(())
}

#[test]
fn manual_completion_and_both_cancels_write_their_rows() -> Result<()> {
    let mut r = rig();
    let manual = r.ended(JobStatus::RequiresReview, Some("VIDEO_UNRESOLVED"));
    assert!(r.repo.mark_completed_manually(manual)?);
    let rows = r.outcomes(manual);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1, rows[0].2.as_deref(), rows[0].3.as_deref()),
        ("delivered_manual", true, Some("manual_done"), Some("VIDEO_UNRESOLVED"))
    );

    let cancel = r.job();
    r.lease(cancel);
    assert_eq!(r.repo.cancel_job(cancel)?, CancelOutcome::Cancelled);
    assert_eq!(r.outcomes(cancel).iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["cancelled"]);

    let user = r.repo.create_user("reporter@example.invalid", "correct horse battery", UserRole::User, "Test Reporter", None)?;
    let own = r.enqueue(NewJob { submitted_by_user_id: Some(user), ..NewJob::new("https://www.own.example/v/1", "9_TESTOPOULOS_OWN", "Testopoulos") });
    assert_eq!(r.repo.cancel_own_pending(own, user)?, OwnCancelOutcome::Cancelled);
    let rows = r.outcomes(own);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].0.as_str(), rows[0].1, rows[0].2.as_deref()), ("cancelled", true, Some("cancel")));
    Ok(())
}

#[test]
fn a_discard_writes_its_row_before_the_delete_but_not_for_a_finished_job() -> Result<()> {
    let mut r = rig();
    let review = r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    r.repo.delete_job(review)?;
    assert!(r.repo.get_job(review)?.is_none(), "the job is gone");
    let rows = r.outcomes(review);
    assert_eq!(rows.len(), 1, "its outcome row outlives it");
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1, rows[0].2.as_deref(), rows[0].3.as_deref()),
        ("discarded", true, Some("discard"), Some("NETWORK"))
    );

    // Tidying away a delivered job is not a second ending.
    let done = r.ended(JobStatus::Completed, None);
    r.repo.delete_job(done)?;
    assert!(r.repo.get_job(done)?.is_none());
    assert_eq!(r.outcomes(done).iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["delivered"]);

    // Nor of a cancelled one.
    let cancelled = r.job();
    assert_eq!(r.repo.cancel_job(cancelled)?, CancelOutcome::Cancelled);
    r.repo.delete_job(cancelled)?;
    assert_eq!(r.outcomes(cancelled).iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["cancelled"]);

    // A waiting job that is discarded is an ending.
    let pending = r.job();
    r.repo.delete_job(pending)?;
    assert_eq!(r.outcomes(pending).iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["discarded"]);
    // A job that does not exist writes nothing.
    let before = r.all_outcomes();
    r.repo.delete_job(987_654)?;
    assert_eq!(r.all_outcomes(), before);
    Ok(())
}

#[test]
fn clearing_failed_jobs_writes_one_discarded_row_each() -> Result<()> {
    let mut r = rig();
    let a = r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    let b = r.ended(JobStatus::Failed, Some("PIPELINE_FAILED"));
    let kept = r.ended(JobStatus::Completed, None);
    assert_eq!(r.repo.clear_failed_jobs()?, 2);
    for id in [a, b] {
        let rows = r.outcomes(id);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0.as_str(), rows[0].2.as_deref()), ("discarded", Some("discard")));
    }
    assert_eq!(r.outcomes(kept).len(), 1);
    Ok(())
}

#[test]
fn a_redownload_ends_again_with_a_second_touched_row() -> Result<()> {
    let mut r = rig();
    let id = r.ended(JobStatus::Completed, None);
    assert!(r.repo.redownload_job(id)?);
    r.lease(id);
    assert!(r.repo.finish(id, OWNER, JobStatus::Completed, None, None, None)?);
    let rows = r.outcomes(id);
    assert_eq!(rows.len(), 2, "two deliveries, two rows");
    assert_eq!((rows[0].0.as_str(), rows[0].1), ("delivered", false));
    assert_eq!((rows[1].0.as_str(), rows[1].1, rows[1].2.as_deref()), ("delivered", true, Some("redownload")));
    Ok(())
}

#[test]
fn the_row_says_whether_the_job_came_by_mail() -> Result<()> {
    let r = rig();
    let mail = r.enqueue(NewJob {
        email_message_id: Some("<invented-1@mail.example>".into()),
        ..NewJob::new("https://www.mailsite.example/v/1", "1_TESTOPOULOS_MAIL", "Testopoulos")
    });
    r.lease(mail);
    assert!(r.repo.finish(mail, OWNER, JobStatus::Completed, None, None, None)?);
    assert!(r.outcomes(mail)[0].5, "from_mail");
    // An attachment's address names a message, not a site.
    let attached = r.enqueue(NewJob {
        email_message_id: Some("<invented-2@mail.example>".into()),
        ..NewJob::new("attachment://invented-2@mail.example/AAMk1", "2_TESTOPOULOS_FILE", "Testopoulos")
    });
    r.lease(attached);
    assert!(r.repo.finish(attached, OWNER, JobStatus::Completed, None, None, None)?);
    assert_eq!(r.outcomes(attached)[0].4, None);
    Ok(())
}

// ---------------------------------------------------------------- the report

#[test]
fn the_report_figures_on_a_fixed_set() -> Result<()> {
    let mut r = rig();
    for _ in 0..8 {
        r.ended(JobStatus::Completed, None);
    }
    let touched = r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    assert!(r.repo.retry_job(touched, None)?);
    r.lease(touched);
    assert!(r.repo.finish(touched, OWNER, JobStatus::Completed, None, None, None)?);
    let manual = r.ended(JobStatus::RequiresReview, Some("VIDEO_UNRESOLVED"));
    assert!(r.repo.mark_completed_manually(manual)?);
    for _ in 0..2 {
        let d = r.ended(JobStatus::RequiresReview, Some("VIDEO_UNRESOLVED"));
        r.repo.delete_job(d)?;
    }
    let c = r.job();
    assert_eq!(r.repo.cancel_job(c)?, CancelOutcome::Cancelled);
    // Waiting for a person right now: one in review, one held.
    r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    r.held(&["https://cdn.news.example/v/a.mp4"]);

    let rep = r.repo.autonomy_report(Utc::now() - Duration::days(14))?;
    let t = &rep.totals;
    assert_eq!(
        (t.ended, t.delivered, t.delivered_untouched, t.delivered_manual, t.discarded, t.cancelled),
        (13, 9, 8, 1, 2, 1)
    );
    assert_eq!(format!("{:.1}", rep.untouched_rate.unwrap() * 100.0), "80.0");
    assert_eq!(format!("{:.1}", rep.strict_rate.unwrap() * 100.0), "61.5");
    assert_eq!(rep.manual, rep.totals, "none came by mail");
    assert_eq!(rep.from_mail.ended, 0);
    let errors: Vec<(&str, i64)> = rep.by_first_error.iter().map(|c| (c.key.as_str(), c.count)).collect();
    assert_eq!(errors, [("VIDEO_UNRESOLVED", 3), ("(none)", 1), ("NETWORK", 1)]);
    let reasons: Vec<(&str, i64)> = rep.by_touch_reason.iter().map(|c| (c.key.as_str(), c.count)).collect();
    assert_eq!(reasons, [("discard", 2), ("cancel", 1), ("manual_done", 1), ("retry", 1)]);
    assert_eq!(rep.waiting_now, 2);
    assert_eq!(rep.by_site.len(), 13);
    assert!(rep.by_site.iter().all(|s| s.site.ends_with(".example") && !s.site.contains('/')));
    assert_eq!(rep.by_site.iter().map(|s| s.total).sum::<i64>(), 13);
    assert_eq!(rep.by_site[0].needed_person, 1, "the sites that needed a person come first");
    assert_eq!(rep.by_site.iter().filter(|s| s.needed_person > 0).count(), 5);
    Ok(())
}

#[test]
fn an_empty_period_has_no_rates_and_no_division_by_zero() -> Result<()> {
    let mut r = rig();
    let rep = r.repo.autonomy_report(Utc::now() - Duration::days(14))?;
    assert_eq!((rep.totals.ended, rep.untouched_rate, rep.strict_rate), (0, None, None));
    assert!(rep.by_first_error.is_empty() && rep.by_touch_reason.is_empty() && rep.by_site.is_empty());

    // Only a cancel: an ending, but nothing reached air.
    let c = r.job();
    assert_eq!(r.repo.cancel_job(c)?, CancelOutcome::Cancelled);
    let rep = r.repo.autonomy_report(Utc::now() - Duration::days(14))?;
    assert_eq!((rep.totals.ended, rep.untouched_rate, rep.strict_rate), (1, None, Some(0.0)));
    Ok(())
}

#[test]
fn since_filters_by_the_ending_time() -> Result<()> {
    let mut r = rig();
    let old = r.ended(JobStatus::Completed, None);
    r.ended(JobStatus::Completed, None);
    r.raw().execute(
        "UPDATE job_outcomes SET ended_at = ? WHERE job_id = ?",
        params![timestamps::format(Utc::now() - Duration::days(20)), old],
    )?;
    assert_eq!(r.repo.autonomy_report(Utc::now() - Duration::days(14))?.totals.ended, 1);
    assert_eq!(r.repo.autonomy_report(Utc::now() - Duration::days(30))?.totals.ended, 2);
    Ok(())
}

#[test]
fn no_address_slug_or_name_reaches_the_outcome_rows_or_the_report() -> Result<()> {
    let r = rig();
    let url = "https://video.kritiki-example.gr/2026/11/secret-harbour-story?id=77";
    let id = r.enqueue(NewJob {
        keyword: "LIMANAKI".into(),
        ..NewJob::new(url, "1_ZACHARIOUDAKIS_LIMANAKI", "Zacharioudakis")
    });
    r.lease(id);
    assert!(r.repo.finish(id, OWNER, JobStatus::RequiresReview, Some("NETWORK"), Some(&format!("timed out on {url}")), None)?);
    assert!(r.repo.retry_job(id, None)?);
    r.lease(id);
    assert!(r.repo.finish(id, OWNER, JobStatus::Completed, None, None, Some("C:/Watch/1_ZACHARIOUDAKIS_LIMANAKI.mxf"))?);
    assert!(r.repo.redownload_job(id)?);
    r.repo.delete_job(id)?;
    // A link with a query and no path.
    let bare = r.enqueue(NewJob::new("https://kritiki-example.gr?token=hidden-token-99", "2_ZACHARIOUDAKIS_LIMANAKI", "Zacharioudakis"));
    r.lease(bare);
    assert!(r.repo.finish(bare, OWNER, JobStatus::Completed, None, None, None)?);

    let mut stored = String::new();
    let conn = r.raw();
    let mut stmt = conn.prepare("SELECT * FROM job_outcomes")?;
    let cols = stmt.column_count();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for i in 0..cols {
            let v: rusqlite::types::Value = row.get(i)?;
            stored.push_str(&format!("{v:?}|"));
        }
    }
    let report = serde_json::to_string(&r.repo.autonomy_report(Utc::now() - Duration::days(1))?)?;
    for text in [&stored, &report] {
        for secret in [
            "secret-harbour", "ZACHARIOUDAKIS", "Zacharioudakis", "LIMANAKI", "2026/11", "id=77", ".mxf", "video.kritiki",
            "token", "hidden-token-99", "?",
        ] {
            assert!(!text.contains(secret), "{secret:?} leaked into {text}");
        }
        assert!(text.contains("kritiki-example.gr"), "the registrable domain is the site: {text}");
    }
    Ok(())
}

// ---------------------------------------------------------------- retention

#[test]
fn outcome_rows_are_kept_400_days() -> Result<()> {
    let mut r = rig();
    let old = r.ended(JobStatus::Completed, None);
    let young = r.ended(JobStatus::Completed, None);
    let now = Utc::now();
    let set = |id: i64, days: i64| {
        r.raw()
            .execute(
                "UPDATE job_outcomes SET ended_at = ? WHERE job_id = ?",
                params![timestamps::format(now - Duration::days(days)), id],
            )
            .unwrap();
    };
    set(old, 401);
    set(young, 399);
    let c = r.repo.purge_old_records(now)?;
    assert_eq!(c.outcomes, 1);
    assert_eq!(r.outcomes(old).len(), 0, "401 days old goes");
    assert_eq!(r.outcomes(young).len(), 1, "399 days old stays");
    assert_eq!(OUTCOME_KEEP_DAYS, 400);
    Ok(())
}

// ---------------------------------------------------------------- a broken statistics table

/// A statistic must never undo a delivery: with every write to `job_outcomes`
/// failing, each ending and operator action still succeeds exactly as before.
#[test]
fn a_failing_outcome_write_never_fails_an_ending_or_an_action() -> Result<()> {
    let mut r = rig();
    // Set up everything that needs a working table first.
    let held = r.held(&["https://cdn.news.example/v/a.mp4"]);
    assert!(r.repo.approve_held(held, &sha(), "desk")?);
    let wrong = r.auto_delivered();
    r.break_outcomes();

    // The worker's finish: the video is in the watchfolder and the job says so.
    let plain = r.job();
    r.lease(plain);
    assert!(r.repo.set_delivered_file(plain, OWNER, "watch/1_TESTOPOULOS_TEST.mxf", 12.5)?);
    assert!(r.repo.finish(plain, OWNER, JobStatus::Completed, None, None, None)?, "finish failed on a statistic");
    let job = r.repo.get_job(plain)?.unwrap();
    assert_eq!((job.status, job.stage, job.lease_owner.as_deref()), (JobStatus::Completed, JobStage::Done, None));
    assert_eq!(job.file_path.as_deref(), Some("watch/1_TESTOPOULOS_TEST.mxf"));
    assert!(job.delivered_at.is_some() && job.completed_at.is_some());

    // The approved held video's delivery.
    r.lease(held);
    assert!(r.repo.record_held_delivery(held, OWNER, "watch/a.mxf", 2.0)?);
    assert!(r.repo.finish_held_delivery(held, OWNER)?);
    assert_eq!(r.repo.get_job(held)?.unwrap().status, JobStatus::Completed);
    assert_eq!(r.repo.get_held(held)?.unwrap().decision.as_deref(), Some("delivered"));

    // Start-up recovery of a job that had reached ARCHIVE.
    let archived = r.job();
    r.lease(archived);
    assert!(r.repo.set_stage(archived, OWNER, JobStage::Archive)?);
    assert_eq!(r.repo.recover_on_startup("testhost")?, 0);
    assert_eq!(r.repo.get_job(archived)?.unwrap().status, JobStatus::Completed);

    // The reaper: two expired leases at ARCHIVE, both finished in the one transaction.
    let (a, b) = (r.job(), r.job());
    for id in [a, b] {
        assert!(r.repo.lease_job_by_id(id, "otherhost:1:0", -60)?);
        assert!(r.repo.set_stage(id, "otherhost:1:0", JobStage::Archive)?);
    }
    assert_eq!(r.repo.reap_expired_leases()?, (0, 0));
    for id in [a, b] {
        assert_eq!(r.repo.get_job(id)?.unwrap().status, JobStatus::Completed, "job {id}");
    }

    // The operator's actions.
    let manual = r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    assert!(r.repo.mark_completed_manually(manual)?);
    assert_eq!(r.repo.get_job(manual)?.unwrap().status, JobStatus::CompletedManual);
    let cancel = r.job();
    assert_eq!(r.repo.cancel_job(cancel)?, CancelOutcome::Cancelled);
    assert_eq!(r.repo.get_job(cancel)?.unwrap().status, JobStatus::Cancelled);
    let discard = r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    r.repo.delete_job(discard)?;
    assert!(r.repo.get_job(discard)?.is_none());
    r.ended(JobStatus::Failed, Some("PIPELINE_FAILED"));
    r.ended(JobStatus::RequiresReview, Some("NETWORK"));
    assert_eq!(r.repo.clear_failed_jobs()?, 2);
    assert_eq!(r.repo.wrong_video_after_auto_delivery(wrong, "desk", Utc::now())?, WrongVideoOutcome::Recorded);
    assert_eq!(r.touch(wrong).1.as_deref(), Some("wrong_video"), "the queue's own touch still lands");

    // Only the rows written before the fault exist, unchanged.
    assert_eq!(r.all_outcomes(), 1, "the auto-delivered job's row from before the fault");
    assert!(!r.outcomes(wrong)[0].1, "its rewrite failed and rolled back alone");
    Ok(())
}

#[test]
fn a_missing_outcome_table_never_fails_a_delivery() -> Result<()> {
    let mut r = rig();
    r.raw().execute_batch("ALTER TABLE job_outcomes RENAME TO job_outcomes_gone")?;
    let id = r.job();
    r.lease(id);
    assert!(r.repo.finish(id, OWNER, JobStatus::Completed, None, None, Some("watch/x.mxf"))?);
    let job = r.repo.get_job(id)?.unwrap();
    assert_eq!((job.status, job.file_path.as_deref()), (JobStatus::Completed, Some("watch/x.mxf")));
    Ok(())
}

// ---------------------------------------------------------------- the site column

#[test]
fn the_site_is_the_plain_hosts_registrable_domain_or_nothing() {
    let r = rig();
    let site = |url: &str| r.site_of(url);
    assert_eq!(site("https://news-a.example?token=abc").as_deref(), Some("news-a.example"), "query and no path");
    assert_eq!(site("https://news-b.example#top").as_deref(), Some("news-b.example"), "fragment and no path");
    assert_eq!(site("https://user:secret@news-c.example/x").as_deref(), Some("news-c.example"), "credentials");
    assert_eq!(site("https://www.news-d.example:8443/v/1").as_deref(), Some("news-d.example"), "port");
    assert_eq!(site("HTTPS://WWW.News-E.Example/v").as_deref(), Some("news-e.example"), "case");
    assert_eq!(site("https://video.portal.com.gr/v").as_deref(), Some("portal.com.gr"), "two-label suffix");
    assert_eq!(site("https://192.0.2.7/v.mp4"), None, "an IPv4 literal is no site");
    assert_eq!(site("https://[2001:db8::1]/v.mp4"), None, "an IPv6 literal is no site");
    assert_eq!(site("attachment://invented-3@mail.example/AAMk2"), None, "an attachment");
    assert_eq!(site("ftp://files.example/v.mp4"), None, "not a web link");
}

// ---------------------------------------------------------------- first error code on every path

#[test]
fn every_path_that_sets_an_error_code_records_the_first_one() -> Result<()> {
    let mut r = rig();
    let first = |r: &Rig, id: i64| r.touch(id).2;

    let rejected = r.held(&["https://cdn.news.example/v/a.mp4"]);
    assert!(r.repo.reject_held(rejected, "not_usable", "no", "desk")?);
    assert_eq!(first(&r, rejected).as_deref(), Some("REJECTED_BY_MCR"));

    let expired = r.held(&["https://cdn.news.example/v/b.mp4"]);
    assert!(r.repo.expire_held(expired, Utc::now())?);
    assert_eq!(first(&r, expired).as_deref(), Some("APPROVAL_EXPIRED"));

    let reviewed = r.at_deliver();
    assert!(r.repo.review_waiting_delivery(reviewed, OWNER, "DELIVERY_UNCONFIRMED", "check Dalet")?);
    assert_eq!(first(&r, reviewed).as_deref(), Some("DELIVERY_UNCONFIRMED"));

    // An earlier code stays the first one.
    let earlier = r.job();
    r.lease(earlier);
    assert!(r.repo.requeue_retry(earlier, OWNER, Duration::seconds(0), "NETWORK", "reset", 4)?);
    r.raw().execute("UPDATE queue SET not_before = NULL WHERE id = ?", params![earlier])?;
    r.lease(earlier);
    assert!(r.repo.review_waiting_delivery(earlier, OWNER, "PIPELINE_FAILED", "unreadable")?);
    assert_eq!(first(&r, earlier).as_deref(), Some("NETWORK"));

    // The panic during delivery, review branch: no marker, nothing provable.
    let panicked = r.at_deliver();
    assert_eq!(r.repo.settle_after_delivery_panic(panicked, OWNER)?, DeliverySettled::Review);
    assert_eq!(first(&r, panicked).as_deref(), Some("DELIVERY_UNCONFIRMED"));

    // Start-up recovery of a marker the share cannot answer about: it waits as DELIVERY_FAILED.
    let unknown = r.at_deliver();
    let gone = r._dir.path().join("share-that-is-down").join("1_TESTOPOULOS_TEST.mxf");
    assert!(r.repo.mark_delivering(unknown, OWNER, &gone.to_string_lossy(), 4096)?);
    assert_eq!(r.repo.recover_on_startup("testhost")?, 1);
    let job = r.repo.get_job(unknown)?.unwrap();
    assert!(job.delivery_wait);
    assert_eq!(job.first_error_code.as_deref(), Some("DELIVERY_FAILED"));
    Ok(())
}

// ---------------------------------------------------------------- marker recoveries count once

#[test]
fn marker_recoveries_and_a_settled_panic_write_one_delivered_row_each() -> Result<()> {
    let mut r = rig();
    // MarkerCheck::Delivered: the file is at the marker's path with its size.
    let delivered = r.at_deliver();
    let file = r.delivered_file("1_DELIVERED.mxf", 4096);
    assert!(r.repo.mark_delivering(delivered, OWNER, &file, 4096)?);
    // MarkerCheck::Recorded: the delivery was recorded, MCR already took the file.
    let recorded = r.at_deliver();
    let taken = r.delivered_file("2_RECORDED.mxf", 2048);
    assert!(r.repo.mark_delivering(recorded, OWNER, &taken, 2048)?);
    assert!(r.repo.set_delivered_file(recorded, OWNER, &taken, 9.0)?);
    std::fs::remove_file(&taken)?;

    assert_eq!(r.repo.recover_on_startup("testhost")?, 0);
    for id in [delivered, recorded] {
        assert_eq!(r.repo.get_job(id)?.unwrap().status, JobStatus::Completed);
        assert_eq!(r.outcomes(id).iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["delivered"], "job {id}");
    }
    assert_eq!(r.repo.recover_on_startup("testhost")?, 0, "nothing left to recover");

    // A panic at DELIVER after the rename: settled as delivered, once.
    let settled = r.at_deliver();
    let file = r.delivered_file("3_SETTLED.mxf", 1024);
    assert!(r.repo.mark_delivering(settled, OWNER, &file, 1024)?);
    assert_eq!(r.repo.settle_after_delivery_panic(settled, OWNER)?, DeliverySettled::Completed);
    assert_eq!(r.repo.settle_after_delivery_panic(settled, OWNER)?, DeliverySettled::NotOurs);
    assert_eq!(r.repo.recover_on_startup("testhost")?, 0);
    assert_eq!(r.repo.reap_expired_leases()?, (0, 0));
    assert_eq!(r.outcomes(settled).iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["delivered"]);
    Ok(())
}
