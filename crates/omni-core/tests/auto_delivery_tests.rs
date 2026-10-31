//! When a repair recipe may deliver without MCR (P10.16a): `recipe_trusted_on`,
//! and «Λάθος βίντεο» on a video that was delivered that way. Real SQLite files;
//! every name and address is invented.

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use tempfile::TempDir;

use omni_core::models::{JobStatus, NewJob};
use omni_core::recipe_score::{Event, InsertOutcome, NewRecipe, Origin, ScopeKind};
use omni_core::repository::{AttemptEnd, Repository, WrongVideoOutcome, DEFAULT_DEDUP_WINDOW_HOURS};

const DOMAIN: &str = "news.example";
const URL: &str = "https://cdn.news.example/v/a.mp4?token=abc";

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
    fn raw(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    fn recipe(&self, tag: &str, origin: Origin) -> (i64, String) {
        let new = NewRecipe {
            scope_kind: ScopeKind::Site,
            scope_key: DOMAIN.into(),
            family: None,
            recipe_json: "{}".into(),
            hash: format!("hash-{tag}"),
            steps_hash: format!("steps-{tag}"),
            dsl_version: 1,
            origin,
        };
        match self.repo.insert_recipe(&new, Utc::now()).unwrap() {
            InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) => (id, new.hash),
            other => panic!("recipe not stored: {other:?}"),
        }
    }

    fn approvals(&self, rid: i64, domain: &str, n: usize) {
        for _ in 0..n {
            self.repo.recipe_event(rid, domain, Event::Approval, Utc::now()).unwrap();
        }
    }

    /// A COMPLETED job whose latest repair attempt delivered without approval.
    fn auto_delivered(&self, n: u32, recipe: Option<(i64, &str)>, outcome: &str) -> i64 {
        let id = self
            .repo
            .enqueue(&NewJob::new(&format!("https://news.example/story-{n}"), &format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();
        assert!(self.repo.lease_job_by_id(id, "w", 180).unwrap());
        let at = self.repo.start_attempt(id, "hold", Utc::now()).unwrap();
        let end = AttemptEnd {
            rung: Some("recipes".into()),
            outcome: Some(outcome.into()),
            recipe_id: recipe.map(|r| r.0),
            recipe_hash: recipe.map(|r| r.1.to_string()),
            domain: Some(DOMAIN.into()),
            held_url: Some(URL.into()),
            ..Default::default()
        };
        self.repo.finish_attempt(at, &end, Utc::now()).unwrap();
        assert!(self.repo.finish(id, "w", JobStatus::Completed, None, None, Some("watch/clip.mxf")).unwrap());
        id
    }
}

#[test]
fn trust_needs_the_approvals_no_rejection_and_an_active_site_row() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("one", Origin::Admin);
    r.approvals(rid, DOMAIN, 4);
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "4 approvals are not enough");
    r.approvals(rid, DOMAIN, 1);
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "5 approvals, no rejection, active");
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 3)?, "a lower bar trusts it too");
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 0)?, "0 means never");
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, -2)?, "a negative value means never");
    assert!(!r.repo.recipe_trusted_on(rid, &hash, "other.example", 5)?, "another domain");
    assert!(!r.repo.recipe_trusted_on(rid, "hash-other", DOMAIN, 5)?, "another recipe behind a reused id");
    assert!(!r.repo.recipe_trusted_on(rid + 100, &hash, DOMAIN, 5)?, "no such recipe");
    Ok(())
}

#[test]
fn a_model_recipe_is_trusted_the_same_way_after_real_approvals() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("llm", Origin::Llm);
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?);
    r.approvals(rid, DOMAIN, 5);
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?);
    Ok(())
}

#[test]
fn one_rejection_or_a_suspension_ends_the_trust() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("rej", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?);
    r.repo.recipe_event(rid, DOMAIN, Event::Rejection, Utc::now())?;
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "5 approvals and 1 rejection");

    // Suspended for failing (no rejection) is not trusted either.
    let (fid, fhash) = r.recipe("fail", Origin::Admin);
    r.approvals(fid, DOMAIN, 5);
    for _ in 0..3 {
        r.repo.recipe_event(fid, DOMAIN, Event::CanaryFail, Utc::now())?;
    }
    let state: String = r.raw().query_row("SELECT state FROM recipe_sites WHERE recipe_id = ?", params![fid], |x| x.get(0))?;
    assert_eq!(state, "suspended");
    assert!(!r.repo.recipe_trusted_on(fid, &fhash, DOMAIN, 5)?, "suspended");
    Ok(())
}

#[test]
fn a_retired_recipe_or_barred_steps_are_not_trusted() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("old", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    r.raw().execute("UPDATE recipes SET state = 'retired' WHERE id = ?", params![rid])?;
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "retired");

    let (bid, bhash) = r.recipe("bar", Origin::Admin);
    r.approvals(bid, DOMAIN, 5);
    assert!(r.repo.recipe_trusted_on(bid, &bhash, DOMAIN, 5)?);
    r.raw().execute(
        "INSERT INTO recipe_bars (steps_hash, domain, at) VALUES ('steps-bar', ?, ?)",
        params![DOMAIN, omni_core::timestamps::format(Utc::now())],
    )?;
    assert!(r.repo.recipe_steps_barred("steps-bar", DOMAIN)?);
    assert!(!r.repo.recipe_trusted_on(bid, &bhash, DOMAIN, 5)?, "barred steps");
    Ok(())
}

#[test]
fn an_auto_delivery_is_a_use_not_an_approval() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("use", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    r.repo.recipe_event(rid, DOMAIN, Event::AutoDelivered, Utc::now())?;
    assert_eq!(r.repo.recipe_site_approvals(rid, DOMAIN)?, 5);
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?);
    Ok(())
}

#[test]
fn wrong_video_revokes_the_trust_once_and_skips_the_address() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("wv", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    let id = r.auto_delivered(1, Some((rid, &hash)), "auto_delivered");

    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::Recorded);
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "trust is gone at once");
    let (state, rejections, reason): (String, i64, Option<String>) = r.raw().query_row(
        "SELECT state, rejections, suspended_reason FROM recipe_sites WHERE recipe_id = ? AND domain = ?",
        params![rid, DOMAIN],
        |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)),
    )?;
    assert_eq!((state.as_str(), rejections, reason.as_deref()), ("suspended", 1, Some("rejected")));
    assert_eq!(r.repo.repair_skip_patterns(DOMAIN, Utc::now())?, vec!["cdn.news.example/v/a.mp4".to_string()]);
    assert_eq!(r.repo.get_job(id)?.unwrap().status, JobStatus::Completed, "the job and its file are not touched");
    let events = r.repo.get_job_events(id, 50)?;
    assert!(events.iter().any(|e| e.message.contains("Dalet")), "a job event says the file is on air");

    // A second press does nothing.
    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::AlreadyDone);
    let rejections: i64 = r.raw().query_row("SELECT rejections FROM recipe_sites WHERE recipe_id = ?", params![rid], |x| x.get(0))?;
    assert_eq!(rejections, 1, "one rejection, not two");
    assert_eq!(r.repo.get_job_events(id, 50)?.len(), events.len(), "and no second event");
    Ok(())
}

#[test]
fn wrong_video_on_a_job_that_was_not_auto_delivered_changes_nothing() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("held", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    let id = r.auto_delivered(1, Some((rid, &hash)), "held");
    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::NotAutoDelivered);
    assert_eq!(r.repo.wrong_video_after_auto_delivery(9999, "desk", Utc::now())?, WrongVideoOutcome::NotAutoDelivered, "no such job");
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "trust untouched");
    assert!(r.repo.repair_skip_patterns(DOMAIN, Utc::now())?.is_empty());
    Ok(())
}

#[test]
fn a_site_row_with_a_rejection_is_not_trusted_even_when_active_with_five_approvals() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("raw", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?);
    r.raw().execute("UPDATE recipe_sites SET rejections = 1, state = 'active' WHERE recipe_id = ?", params![rid])?;
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?);
    Ok(())
}

#[test]
fn any_mcr_rejection_of_the_recipes_video_on_the_site_ends_the_trust() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("na", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    // MCR rejected a video of this recipe as "not usable": the site row learns nothing (rejections stay 0).
    let id = r.auto_delivered(1, Some((rid, &hash)), "held");
    r.raw().execute(
        "UPDATE repair_attempts SET mcr_decision = 'rejected', mcr_reason = 'not_usable' WHERE job_id = ?",
        params![id],
    )?;
    let (state, rejections): (String, i64) =
        r.raw().query_row("SELECT state, rejections FROM recipe_sites WHERE recipe_id = ?", params![rid], |x| Ok((x.get(0)?, x.get(1)?)))?;
    assert_eq!((state.as_str(), rejections), ("active", 0));
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "not usable ends the trust");
    // Another domain's rejection does not.
    let (oid, ohash) = r.recipe("na2", Origin::Admin);
    r.approvals(oid, DOMAIN, 5);
    let other = r.auto_delivered(2, Some((oid, &ohash)), "held");
    r.raw().execute(
        "UPDATE repair_attempts SET mcr_decision = 'rejected', mcr_reason = 'other', domain = 'else.example' WHERE job_id = ?",
        params![other],
    )?;
    assert!(r.repo.recipe_trusted_on(oid, &ohash, DOMAIN, 5)?, "a rejection on another site is not this site's");
    Ok(())
}

#[test]
fn a_redownload_takes_the_wrong_video_button_away() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("redo", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    let id = r.auto_delivered(1, Some((rid, &hash)), "auto_delivered");
    assert!(r.repo.redownload_job(id)?);
    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::NotAutoDelivered);
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "the recipe is untouched");
    let at = r.repo.latest_attempt(id)?.unwrap();
    assert_eq!(at.mcr_decision.as_deref(), Some("superseded"));
    Ok(())
}

// ------------------- the marker written before the delivery, and the purge (P10.16b) --

#[test]
fn the_purge_keeps_an_automatic_delivery_of_a_job_that_still_exists() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("purge", Origin::Admin);
    let delivered = r.auto_delivered(1, Some((rid, &hash)), "auto_delivered");
    let delivering = r.auto_delivered(2, Some((rid, &hash)), "auto_delivering");
    let held = r.auto_delivered(3, Some((rid, &hash)), "held");
    r.raw().execute("UPDATE repair_attempts SET started_at = '2020-01-01T00:00:00.000Z'", [])?;

    assert_eq!(r.repo.purge_repair_attempts(90)?, 1, "only the held one is old news");
    assert!(r.repo.latest_attempt(delivered)?.is_some(), "the card and the «Νέα λήψη» guard stay");
    assert!(r.repo.latest_attempt(delivering)?.is_some());
    assert!(r.repo.latest_attempt(held)?.is_none());
    // The guard still works after the purge.
    assert!(r.repo.redownload_job(delivered)?);
    assert!(r.repo.job_auto_delivery_superseded(delivered)?);
    // Once the job is gone, so is the row.
    r.repo.delete_job(delivering)?;
    let left: i64 = r.raw().query_row("SELECT COUNT(*) FROM repair_attempts WHERE job_id = ?", params![delivering], |x| x.get(0))?;
    assert_eq!(left, 0);
    Ok(())
}

#[test]
fn an_auto_delivering_marker_on_a_completed_job_is_an_automatic_delivery() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("marker", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    let id = r.auto_delivered(1, Some((rid, &hash)), "auto_delivering");
    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::Recorded);
    assert!(!r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?);
    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::AlreadyDone);

    // A redownload supersedes it like an 'auto_delivered' one.
    let (oid, ohash) = r.recipe("marker2", Origin::Admin);
    let other = r.auto_delivered(2, Some((oid, &ohash)), "auto_delivering");
    assert!(r.repo.redownload_job(other)?);
    assert!(r.repo.job_auto_delivery_superseded(other)?);
    Ok(())
}

#[test]
fn an_auto_delivering_marker_on_a_job_that_is_not_completed_shows_nothing() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("running", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    let id = r.repo.enqueue(&NewJob::new("https://news.example/story-9", "9_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
    assert!(r.repo.lease_job_by_id(id, "w", 180)?);
    let at = r.repo.start_attempt(id, "hold", Utc::now())?;
    r.repo.mark_attempt_auto_delivering(at, rid, &hash, DOMAIN, URL)?;
    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::NotAutoDelivered);
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "nothing was learned");
    Ok(())
}

#[test]
fn a_crash_after_the_watchfolder_rename_leaves_an_automatic_delivery_and_tells_mcr() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("crash", Origin::Admin);
    let owner = "mcr-pc:1:0";
    let id = r.repo.enqueue(&NewJob::new("https://news.example/story-7", "7_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
    assert!(r.repo.lease_job_by_id(id, owner, 3600)?);
    let at = r.repo.start_attempt(id, "hold", Utc::now())?;
    r.repo.mark_attempt_auto_delivering(at, rid, &hash, DOMAIN, URL)?;
    let row = r.repo.latest_attempt(id)?.unwrap();
    assert_eq!((row.outcome.as_deref(), row.recipe_id, row.domain.as_deref(), row.held_url.as_deref()), (Some("auto_delivering"), Some(rid), Some(DOMAIN), Some(URL)));
    assert!(r.repo.set_stage(id, owner, omni_core::models::JobStage::Archive)?);
    // A control: a job in the same state with a plain attempt gets no such note.
    let plain = r.repo.enqueue(&NewJob::new("https://news.example/story-8", "8_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
    assert!(r.repo.lease_job_by_id(plain, "mcr-pc:1:1", 3600)?);
    r.repo.start_attempt(plain, "hold", Utc::now())?;
    assert!(r.repo.set_stage(plain, "mcr-pc:1:1", omni_core::models::JobStage::Archive)?);

    assert_eq!(r.repo.recover_on_startup("mcr-pc")?, 0, "both are finished, none requeued");
    assert_eq!(r.repo.get_job(id)?.unwrap().status, JobStatus::Completed);
    assert_eq!(r.repo.latest_attempt(id)?.unwrap().outcome.as_deref(), Some("auto_delivered"));
    let msg = |j: i64| r.repo.get_job_events(j, 50).unwrap().iter().any(|e| e.message.contains("ανάκτηση μετά από διακοπή"));
    assert!(msg(id), "the job log says so");
    assert!(!msg(plain));
    let audit = r.repo.get_recent_audit_logs(20)?;
    assert!(audit.iter().any(|a| a.category == "REPAIR" && a.message.contains(&format!("Job {id}"))), "an audit row says so");
    // And MCR can call it the wrong video.
    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::Recorded);
    Ok(())
}

#[test]
fn a_crash_before_the_rename_never_reads_as_an_automatic_delivery_later() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("early", Origin::Admin);
    r.approvals(rid, DOMAIN, 5);
    let owner = "mcr-pc:1:0";
    let id = r.repo.enqueue(&NewJob::new("https://news.example/story-9", "9_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
    assert!(r.repo.lease_job_by_id(id, owner, 3600)?);
    let at = r.repo.start_attempt(id, "hold", Utc::now())?;
    r.repo.mark_attempt_auto_delivering(at, rid, &hash, DOMAIN, URL)?;
    // The daemon dies while transcoding: nothing reached the watchfolder.
    assert!(r.repo.set_stage(id, owner, omni_core::models::JobStage::Transcode)?);
    assert_eq!(r.repo.recover_on_startup("mcr-pc")?, 1, "requeued");
    assert_eq!(r.repo.latest_attempt(id)?.unwrap().outcome.as_deref(), Some("interrupted"));

    // The next run delivers on the normal path, writing no attempt of its own.
    assert!(r.repo.lease_job_by_id(id, "mcr-pc:2:0", 3600)?);
    assert!(r.repo.finish(id, "mcr-pc:2:0", JobStatus::Completed, None, None, Some("watch/clip.mxf"))?);
    assert_eq!(r.repo.wrong_video_after_auto_delivery(id, "desk", Utc::now())?, WrongVideoOutcome::NotAutoDelivered);
    assert!(r.repo.recipe_trusted_on(rid, &hash, DOMAIN, 5)?, "the recipe that did not deliver is not punished");
    let skips = r.repo.repair_skip_patterns(DOMAIN, Utc::now())?;
    assert!(skips.is_empty(), "no skip for an address that never went on air");
    Ok(())
}

#[test]
fn a_new_lease_ends_a_marker_the_earlier_run_left_behind() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("lease", Origin::Admin);
    let id = r.repo.enqueue(&NewJob::new("https://news.example/story-10", "10_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
    assert!(r.repo.lease_job_by_id(id, "mcr-pc:1:0", 3600)?);
    let at = r.repo.start_attempt(id, "hold", Utc::now())?;
    r.repo.mark_attempt_auto_delivering(at, rid, &hash, DOMAIN, URL)?;
    // MCR cancelled it and the daemon died before the worker settled the attempt; later it is retried.
    r.raw().execute("UPDATE queue SET status = 'PENDING', lease_owner = NULL WHERE id = ?", params![id])?;
    assert!(r.repo.lease_job_by_id(id, "mcr-pc:2:0", 3600)?);
    assert_eq!(r.repo.latest_attempt(id)?.unwrap().outcome.as_deref(), Some("interrupted"));
    Ok(())
}

#[test]
fn a_crash_at_deliver_warns_mcr_that_the_video_may_be_on_air() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("deliver", Origin::Admin);
    let owner = "mcr-pc:1:0";
    let id = r.repo.enqueue(&NewJob::new("https://news.example/story-11", "11_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
    assert!(r.repo.lease_job_by_id(id, owner, 3600)?);
    let at = r.repo.start_attempt(id, "hold", Utc::now())?;
    r.repo.mark_attempt_auto_delivering(at, rid, &hash, DOMAIN, URL)?;
    assert!(r.repo.set_stage(id, owner, omni_core::models::JobStage::Deliver)?);
    // A control at TRANSCODE gets no such warning.
    let early = r.repo.enqueue(&NewJob::new("https://news.example/story-12", "12_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
    assert!(r.repo.lease_job_by_id(early, "mcr-pc:1:1", 3600)?);
    let at2 = r.repo.start_attempt(early, "hold", Utc::now())?;
    r.repo.mark_attempt_auto_delivering(at2, rid, &hash, DOMAIN, URL)?;
    assert!(r.repo.set_stage(early, "mcr-pc:1:1", omni_core::models::JobStage::Transcode)?);

    assert_eq!(r.repo.recover_on_startup("mcr-pc")?, 2, "both requeued");
    let warned = |j: i64| r.repo.get_job_events(j, 50).unwrap().iter().any(|e| e.message.contains("ελέγξτε αν το βίντεο είναι ήδη στο Dalet"));
    assert!(warned(id));
    assert!(!warned(early));
    let audit = r.repo.get_recent_audit_logs(20)?;
    assert!(audit.iter().any(|a| a.category == "REPAIR" && a.message.contains(&format!("Job {id}: interrupted at DELIVER"))));
    assert_eq!(r.repo.latest_attempt(id)?.unwrap().outcome.as_deref(), Some("interrupted"));
    Ok(())
}
