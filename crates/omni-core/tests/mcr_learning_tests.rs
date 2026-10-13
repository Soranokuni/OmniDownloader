//! What MCR's decisions on a held repaired video teach the recipe store and the
//! skip list (plan P10.13), and «Επόμενη επιλογή». Real SQLite files; every name
//! and address is invented.

use anyhow::Result;
use chrono::{Duration, Utc};
use rusqlite::{params, Connection};
use tempfile::TempDir;

use omni_core::models::{JobStatus, NewJob};
use omni_core::recipe_score::{InsertOutcome, NewRecipe, Origin, ScopeKind};
use omni_core::repository::{AttemptEnd, Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const DOMAIN: &str = "news.example";
const SHA: &str = "ab";
const A: &str = "https://cdn.news.example/v/a.mp4";
const B: &str = "https://cdn.news.example/v/b.mp4?token=abc";
const C: &str = "https://cdn.news.example/v/c.mp4";
const D: &str = "https://cdn.news.example/v/d.mp4";

fn sha() -> String {
    SHA.repeat(32)
}

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

    /// (state, approvals, rejections, suspended_reason) of the recipe on the domain; `None` = no row.
    fn site(&self, recipe_id: i64) -> Option<(String, i64, i64, Option<String>)> {
        self.raw()
            .query_row(
                "SELECT state, approvals, rejections, suspended_reason FROM recipe_sites WHERE recipe_id = ? AND domain = ?",
                params![recipe_id, DOMAIN],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()
    }

    fn barred(&self, steps: &str) -> bool {
        self.repo.recipe_steps_barred(steps, DOMAIN).unwrap()
    }

    fn skips(&self) -> Vec<String> {
        self.repo.repair_skip_patterns(DOMAIN, Utc::now()).unwrap()
    }

    fn tombstones(&self) -> i64 {
        self.raw().query_row("SELECT COUNT(*) FROM recipe_tombstones", [], |r| r.get(0)).unwrap()
    }

    /// A job leased and parked for approval with a repair attempt that held `urls[index]`
    /// (the list is the attempt's verified candidates). Returns the job id.
    fn held(&self, n: u32, recipe: Option<(i64, &str)>, urls: &[&str], index: usize) -> i64 {
        let id = self
            .repo
            .enqueue(&NewJob::new(&format!("https://news.example/story-{n}"), &format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();
        assert!(self.repo.lease_job_by_id(id, "w", 180).unwrap());
        self.hold_again(id, recipe, urls, index);
        id
    }

    /// The leased job `id` holds `urls[index]` through a new repair attempt.
    fn hold_again(&self, id: i64, recipe: Option<(i64, &str)>, urls: &[&str], index: usize) {
        let at = self.repo.start_attempt(id, "hold", Utc::now()).unwrap();
        let list: Vec<serde_json::Value> = urls.iter().map(|u| serde_json::json!({ "url": u, "origin": "recipe:x" })).collect();
        let end = AttemptEnd {
            rung: Some("recipes".into()),
            outcome: Some("held".into()),
            recipe_id: recipe.map(|r| r.0),
            recipe_hash: recipe.map(|r| r.1.to_string()),
            domain: Some(DOMAIN.into()),
            candidates_json: Some(serde_json::Value::Array(list).to_string()),
            held_url: Some(urls[index].to_string()),
            next_index: index as i64,
            ..Default::default()
        };
        self.repo.finish_attempt(at, &end, Utc::now()).unwrap();
        assert!(self.repo.hold_output(id, "w", "temp/held/x/clip.mxf", 10, &sha(), Some(9.0), Utc::now() + Duration::hours(24)).unwrap());
        assert!(self.repo.finish(id, "w", JobStatus::AwaitingApproval, None, None, None).unwrap());
    }
}

#[test]
fn approving_a_held_video_records_an_approval_for_its_recipe_on_that_domain() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("one", Origin::Llm);
    assert!(r.site(rid).is_none(), "no row before MCR says anything");
    let id = r.held(1, Some((rid, &hash)), &[A], 0);
    assert!(r.repo.approve_held(id, &sha(), "desk")?);
    let (state, approvals, rejections, _) = r.site(rid).expect("a row after the approval");
    assert_eq!((state.as_str(), approvals, rejections), ("active", 1, 0), "a candidate becomes active");
    assert_eq!(r.repo.recipe_site_approvals(rid, DOMAIN)?, 1);
    assert!(r.skips().is_empty());
    Ok(())
}

#[test]
fn a_wrong_video_suspends_the_recipe_there_bars_its_steps_and_skips_the_address() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("known", Origin::Admin);
    // MCR approved it once before, so it is a known recipe.
    let first = r.held(1, Some((rid, &hash)), &[A], 0);
    assert!(r.repo.approve_held(first, &sha(), "desk")?);
    assert!(!r.barred("steps-known"));

    let id = r.held(2, Some((rid, &hash)), &[B], 0);
    assert!(r.repo.reject_held(id, "wrong_video", "no", "desk")?);
    let (state, approvals, rejections, reason) = r.site(rid).unwrap();
    assert_eq!((state.as_str(), approvals, rejections, reason.as_deref()), ("suspended", 1, 1, Some("rejected")));
    assert!(r.barred("steps-known"), "its steps are barred on the domain");
    assert_eq!(r.skips(), vec!["cdn.news.example/v/b.mp4".to_string()], "the address, without the query");
    assert_eq!(r.tombstones(), 0, "a recipe with approvals is suspended, not tombstoned");
    Ok(())
}

#[test]
fn a_never_approved_model_recipe_is_tombstoned_and_cannot_be_inserted_again() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("ghost", Origin::Llm);
    let id = r.held(1, Some((rid, &hash)), &[A], 0);
    assert!(r.repo.reject_held(id, "wrong_video", "no", "desk")?);
    assert_eq!(r.tombstones(), 1);
    let again = NewRecipe {
        scope_kind: ScopeKind::Site,
        scope_key: DOMAIN.into(),
        family: None,
        recipe_json: "{}".into(),
        hash,
        steps_hash: "steps-ghost".into(),
        dsl_version: 1,
        origin: Origin::Llm,
    };
    assert_eq!(r.repo.insert_recipe(&again, Utc::now())?, InsertOutcome::Refused("rejected".into()));
    assert_eq!(r.skips().len(), 1);
    Ok(())
}

#[test]
fn not_usable_other_and_expiry_teach_nothing_and_skip_nothing() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("quiet", Origin::Llm);
    let a = r.held(1, Some((rid, &hash)), &[A], 0);
    assert!(r.repo.reject_held(a, "not_usable", "no", "desk")?);
    let b = r.held(2, Some((rid, &hash)), &[B], 0);
    assert!(r.repo.reject_held(b, "other", "no", "desk")?);
    let c = r.held(3, Some((rid, &hash)), &[C], 0);
    assert!(r.repo.expire_held(c, Utc::now())?);
    assert!(r.site(rid).is_none(), "no recipe event");
    assert!(!r.barred("steps-quiet"));
    assert!(r.skips().is_empty(), "no skip");
    assert_eq!(r.tombstones(), 0);
    Ok(())
}

#[test]
fn a_wrong_video_without_a_recipe_still_skips_the_address() -> Result<()> {
    let r = rig();
    let id = r.held(1, None, &[A], 0);
    assert!(r.repo.reject_held(id, "wrong_video", "no", "desk")?);
    assert_eq!(r.skips(), vec!["cdn.news.example/v/a.mp4".to_string()]);
    // A recipe that is gone teaches nothing, but the decision stands.
    let (rid, hash) = r.recipe("gone", Origin::Llm);
    let id2 = r.held(2, Some((rid, &hash)), &[B], 0);
    r.raw().execute("DELETE FROM recipes WHERE id = ?", params![rid])?;
    assert!(r.repo.reject_held(id2, "wrong_video", "no", "desk")?);
    assert_eq!(r.repo.get_job(id2)?.unwrap().status, JobStatus::RequiresReview);
    Ok(())
}

/// The recipe write and the decision share one transaction: a recipe write that
/// fails takes the decision back with it, for approval and for a rejection.
#[test]
fn a_failing_recipe_write_rolls_the_decision_back() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("tx", Origin::Llm);
    let ok = r.held(1, Some((rid, &hash)), &[A], 0);
    let bad = r.held(2, Some((rid, &hash)), &[B], 0);
    r.raw().execute_batch(
        "CREATE TRIGGER no_site_rows BEFORE INSERT ON recipe_sites BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )?;
    assert!(r.repo.approve_held(ok, &sha(), "desk").is_err(), "the recipe write failed");
    assert!(r.repo.reject_held(bad, "wrong_video", "no", "desk").is_err());
    for id in [ok, bad] {
        assert_eq!(r.repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval, "the job is still waiting");
        assert!(r.repo.waiting_held(id)?.is_some(), "the held row is still undecided");
        assert_eq!(r.repo.latest_attempt(id)?.unwrap().mcr_decision, None);
    }
    assert!(r.skips().is_empty(), "the skip was rolled back too");
    // Without the fault the same decisions go through.
    r.raw().execute_batch("DROP TRIGGER no_site_rows")?;
    assert!(r.repo.approve_held(ok, &sha(), "desk")?);
    assert!(r.repo.reject_held(bad, "wrong_video", "no", "desk")?);
    Ok(())
}

#[test]
fn skips_hold_fifty_per_domain_extend_on_repeat_and_expire() -> Result<()> {
    let r = rig();
    let id = r.held(1, None, &["https://cdn.news.example/v/clip-00.mp4"], 0);
    assert!(r.repo.reject_held(id, "wrong_video", "no", "desk")?);
    for n in 1..=52 {
        assert!(r.repo.retry_job(id, None)?);
        assert!(r.repo.lease_job_by_id(id, "w", 180)?);
        r.hold_again(id, None, &[&format!("https://cdn.news.example/v/clip-{n:02}.mp4")], 0);
        assert!(r.repo.reject_held(id, "wrong_video", "no", "desk")?);
    }
    let kept = r.skips();
    assert_eq!(kept.len(), 50, "at most 50 per domain");
    assert!(kept.contains(&"cdn.news.example/v/clip-52.mp4".to_string()), "the newest stays");
    assert!(!kept.contains(&"cdn.news.example/v/clip-00.mp4".to_string()), "the oldest goes");

    // Rejecting a kept address again does not add a row.
    assert!(r.repo.retry_job(id, None)?);
    assert!(r.repo.lease_job_by_id(id, "w", 180)?);
    r.hold_again(id, None, &["https://cdn.news.example/v/clip-52.mp4?t=2"], 0);
    assert!(r.repo.reject_held(id, "wrong_video", "no", "desk")?);
    assert_eq!(r.skips().len(), 50);

    // 30 days: still there after 29, gone from the answer after 31, and retention deletes them.
    let now = Utc::now();
    assert_eq!(r.repo.repair_skip_patterns(DOMAIN, now + Duration::days(29))?.len(), 50);
    assert!(r.repo.repair_skip_patterns(DOMAIN, now + Duration::days(31))?.is_empty());
    assert_eq!(r.repo.purge_repair_skips(now + Duration::days(1))?, 0, "nothing ran out yet");
    assert_eq!(r.repo.purge_repair_skips(now + Duration::days(31))?, 50);
    assert!(r.skips().is_empty());
    Ok(())
}

#[test]
fn next_candidate_rejects_the_held_one_and_queues_the_next_verified_one() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("next", Origin::Llm);
    let id = r.held(1, Some((rid, &hash)), &[A, B, C], 0);
    assert_eq!(r.repo.next_left(id, Utc::now())?, 2);

    // Not the file MCR saw: nothing changes.
    assert_eq!(r.repo.next_candidate(id, &"cd".repeat(32), "desk", Utc::now())?, None);
    assert_eq!(r.repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval);

    assert_eq!(r.repo.next_candidate(id, &sha(), "desk", Utc::now())?, Some(1));
    let job = r.repo.get_job(id)?.unwrap();
    assert_eq!(job.status, JobStatus::Pending);
    let held = r.repo.get_held(id)?.unwrap();
    assert_eq!((held.decision.as_deref(), held.reason.as_deref()), (Some("rejected"), Some("wrong_video")));
    let next = r.repo.latest_attempt(id)?.unwrap();
    assert_eq!((next.rung.as_deref(), next.outcome.as_deref(), next.next_index), (Some("next"), None, 1));
    assert_eq!(next.mode, "hold");
    assert_eq!(next.domain.as_deref(), Some(DOMAIN));
    assert!(next.candidates_json.as_deref().is_some_and(|j| j.contains("b.mp4")), "the verified list is copied");
    // What a wrong-video rejection does.
    assert_eq!(r.repo.rejected_held_urls(id)?, vec![A.to_string()]);
    assert_eq!(r.skips(), vec!["cdn.news.example/v/a.mp4".to_string()]);
    let (state, _, rejections, _) = r.site(rid).unwrap();
    assert_eq!((state.as_str(), rejections), ("suspended", 1));
    assert_eq!(r.tombstones(), 1, "the unapproved model recipe is tombstoned");
    // Not held any more: a second press changes nothing.
    assert_eq!(r.repo.next_candidate(id, &sha(), "desk", Utc::now())?, None);
    Ok(())
}

#[test]
fn next_candidate_passes_over_skipped_and_rejected_addresses_and_says_none_when_nothing_is_left() -> Result<()> {
    let r = rig();
    // #1 (B) was MCR's wrong video on an earlier job of the same site: skipped.
    let other = r.held(1, None, &[B], 0);
    assert!(r.repo.reject_held(other, "wrong_video", "no", "desk")?);
    assert_eq!(r.skips().len(), 1);

    let id = r.held(2, None, &[A, B, C], 0);
    assert_eq!(r.repo.next_left(id, Utc::now())?, 1, "B is skipped, only C is left");
    assert_eq!(r.repo.next_candidate(id, &sha(), "desk", Utc::now())?, Some(2), "the skipped one is passed over");

    // C is held now (the worker's doing). Nothing after it: no next.
    assert!(r.repo.lease_job_by_id(id, "w", 180)?);
    r.hold_again(id, None, &[A, B, C], 2);
    assert_eq!(r.repo.next_left(id, Utc::now())?, 0);
    assert_eq!(r.repo.next_candidate(id, &sha(), "desk", Utc::now())?, None);
    assert_eq!(r.repo.get_job(id)?.unwrap().status, JobStatus::AwaitingApproval, "nothing changed");
    assert!(r.repo.waiting_held(id)?.is_some());

    // An address MCR rejected for this job in an earlier run is not offered either;
    // one it rejected for another job is (only the skip list is shared across jobs).
    let id3 = r.held(3, None, &[D], 0);
    assert!(r.repo.reject_held(id3, "not_usable", "no", "desk")?);
    assert!(r.repo.retry_job(id3, None)?);
    assert!(r.repo.lease_job_by_id(id3, "w", 180)?);
    r.hold_again(id3, None, &[A, D, C], 0);
    assert_eq!(r.repo.next_left(id3, Utc::now())?, 1, "D was rejected for this job before: only C");
    assert_eq!(r.repo.next_candidate(id3, &sha(), "desk", Utc::now())?, Some(2));
    let id4 = r.held(4, None, &[A, D, C], 0);
    assert_eq!(r.repo.next_left(id4, Utc::now())?, 2, "D was rejected for another job, not this one");
    Ok(())
}

#[test]
fn a_retry_drops_a_next_run_that_has_not_started() -> Result<()> {
    let r = rig();
    let id = r.held(1, None, &[A, B], 0);
    assert_eq!(r.repo.next_candidate(id, &sha(), "desk", Utc::now())?, Some(1));
    assert!(r.repo.latest_attempt(id)?.unwrap().outcome.is_none());
    assert!(r.repo.retry_job(id, None)?);
    let latest = r.repo.latest_attempt(id)?.unwrap();
    assert_eq!((latest.rung.as_deref(), latest.outcome.as_deref()), (Some("next"), Some("superseded")), "the retry starts clean");
    Ok(())
}

/// Two copies of one video that differ only in their query: the held one's own skip
/// bars the other, so it is neither counted nor chosen; the press goes to the next different video.
#[test]
fn a_copy_of_the_held_video_with_another_query_is_not_a_next_choice() -> Result<()> {
    let r = rig();
    let a1 = "https://cdn.news.example/v/a.mp4?tok=1";
    let a2 = "https://cdn.news.example/v/a.mp4?tok=2";
    let c = "https://cdn.news.example/v/c.mp4";
    let id = r.held(1, None, &[a1, a2, c], 0);
    assert_eq!(r.repo.next_left(id, Utc::now())?, 1, "only c.mp4 is a different video");
    assert_eq!(r.repo.next_candidate(id, &sha(), "desk", Utc::now())?, Some(2), "the press picks c.mp4");
    Ok(())
}

/// A deleted recipe's id can be reused by another recipe: the decision must not credit it.
#[test]
fn a_reused_recipe_id_gets_no_event_and_no_tombstone() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("old", Origin::Llm);
    let id = r.held(1, Some((rid, &hash)), &[A], 0);
    r.raw().execute("DELETE FROM recipes WHERE id = ?", params![rid])?;
    // The next insert takes the freed id (no AUTOINCREMENT).
    let (other, _) = r.recipe("new", Origin::Llm);
    assert_eq!(other, rid, "the id was reused");
    assert!(r.repo.reject_held(id, "wrong_video", "no", "desk")?);
    assert!(r.site(other).is_none(), "no event on the unrelated recipe");
    assert!(!r.barred("steps-new"));
    assert_eq!(r.tombstones(), 0, "and it is not tombstoned");
    let id2 = r.held(2, Some((other, "hash-new")), &[B], 0);
    assert!(r.repo.approve_held(id2, &sha(), "desk")?);
    assert_eq!(r.site(other).unwrap().1, 1, "a matching hash still teaches");
    Ok(())
}

#[test]
fn a_rejection_stores_its_reason_with_the_decision_and_approval_and_expiry_store_none() -> Result<()> {
    let r = rig();
    let (rid, hash) = r.recipe("why", Origin::Llm);
    let reason_of = |job: i64| -> (Option<String>, Option<String>) {
        r.raw()
            .query_row("SELECT mcr_decision, mcr_reason FROM repair_attempts WHERE job_id = ?", params![job], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
    };
    let a = r.held(1, Some((rid, &hash)), &[A], 0);
    assert!(r.repo.reject_held(a, "wrong_video", "no", "desk")?);
    assert_eq!(reason_of(a), (Some("rejected".into()), Some("wrong_video".into())));
    let b = r.held(2, Some((rid, &hash)), &[B], 0);
    assert!(r.repo.reject_held(b, "other", "no", "desk")?);
    assert_eq!(reason_of(b), (Some("rejected".into()), Some("other".into())));
    let c = r.held(3, Some((rid, &hash)), &[C], 0);
    assert!(r.repo.expire_held(c, Utc::now())?);
    assert_eq!(reason_of(c), (Some("expired".into()), None));
    Ok(())
}
