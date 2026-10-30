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
