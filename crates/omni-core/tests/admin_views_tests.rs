//! What the admin's Sites, Recipes and Repair-report cards read (plan P10.15a).
//!
//! Real SQLite files, invented sites and recipes.

use anyhow::Result;
use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use omni_core::models::NewJob;
use omni_core::recipe_score::{Event, InsertOutcome, NewRecipe, Origin, RetiredReason, ScopeKind, ShippedRecipe};
use omni_core::repository::{AdminRetire, AttemptEnd, Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use omni_core::site_health::{SiteOutcome, SiteState};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap()
}

fn rig() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join("omni.db")).unwrap()
}

fn sha(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn json(n: usize) -> String {
    format!(r#"{{"dsl_version":1,"steps":[{{"op":"ldjson","field":"f{n}"}},{{"op":"media","from":0,"referer":"page"}}]}}"#)
}

fn new_recipe(key: &str, n: usize, origin: Origin) -> NewRecipe {
    let j = json(n);
    NewRecipe {
        scope_kind: ScopeKind::Site,
        scope_key: key.to_string(),
        family: None,
        hash: sha(&format!("site|{key}|{j}")),
        steps_hash: sha(&j),
        recipe_json: j,
        dsl_version: 1,
        origin,
    }
}

fn id_of(o: InsertOutcome) -> i64 {
    match o {
        InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) | InsertOutcome::Revived(id) => id,
        other => panic!("expected an id, got {other:?}"),
    }
}

// ------------------------------------------------------------------ sites

#[test]
fn the_site_list_has_every_row_the_broken_first_with_canary_counts_and_only_a_live_breaker() -> Result<()> {
    let (_d, repo) = rig();
    let now = Utc::now();
    repo.record_site_outcome("calm.example", SiteOutcome::Success)?;
    repo.record_site_outcome("sick.example", SiteOutcome::SiteFailure("NO_VIDEO".into()))?;
    repo.record_site_outcome("dead.example", SiteOutcome::SiteFailure("NO_VIDEO".into()))?;
    assert!(repo.set_site_state("sick.example", SiteState::Degraded)?);
    assert!(repo.set_site_state("dead.example", SiteState::Broken)?);
    repo.open_llm_breaker("dead.example", now + Duration::hours(2))?;
    repo.open_llm_breaker("sick.example", now - Duration::hours(2))?;

    for (i, url) in ["https://dead.example/a", "https://dead.example/b", "https://dead.example/c"].iter().enumerate() {
        repo.add_canary("dead.example", url, None, now - Duration::hours(i as i64))?;
    }
    let ids: Vec<i64> = repo.list_canaries_for("dead.example")?.into_iter().map(|c| c.id).collect();
    repo.record_canary_result(ids[0], true, "ok")?;
    repo.record_canary_result(ids[1], false, "no video found on the page")?;
    repo.mark_canary_gone(ids[2])?; // taken down: counts as neither present nor ok

    let rows = repo.list_sites(now)?;
    let order: Vec<&str> = rows.iter().map(|r| r.site.domain.as_str()).collect();
    assert_eq!(order, ["dead.example", "sick.example", "calm.example"], "broken, degraded, healthy");

    let dead = &rows[0];
    assert_eq!((dead.canaries, dead.canaries_ok), (2, 1), "the gone page is not counted");
    assert!(dead.canary_checked_at.is_some());
    assert!(dead.site.llm_breaker_until.is_some(), "a breaker still running is shown");
    assert!(rows[1].site.llm_breaker_until.is_none(), "an expired breaker is not");
    let calm = &rows[2];
    assert_eq!((calm.canaries, calm.canaries_ok, calm.canary_checked_at), (0, 0, None));
    assert_eq!(calm.site.recent, "1");
    Ok(())
}

// ---------------------------------------------------------------- recipes

#[test]
fn the_recipe_list_carries_scores_per_site_and_the_export_leaves_out_shipped_and_retired() -> Result<()> {
    let (_d, repo) = rig();
    let admin = id_of(repo.insert_recipe(&new_recipe("a.example", 1, Origin::Admin), t0())?);
    let llm = id_of(repo.insert_recipe(&new_recipe("b.example", 2, Origin::Llm), t0())?);
    let gone = id_of(repo.insert_recipe(&new_recipe("c.example", 3, Origin::Llm), t0())?);
    let j = json(900);
    repo.sync_shipped(&[ShippedRecipe { name: "generic-x".into(), hash: sha("gen"), steps_hash: sha(&j), recipe_json: j, dsl_version: 1 }], t0())?;
    repo.recipe_event(admin, "a.example", Event::Approval, t0())?;
    repo.recipe_event(admin, "a.example", Event::Approval, t0())?;
    repo.recipe_event(admin, "z.example", Event::Rejection, t0())?;
    assert_eq!(repo.admin_retire_recipe(gone, t0())?, AdminRetire::Retired);

    let list = repo.list_recipes(t0())?;
    assert_eq!(list.len(), 4, "shipped and retired are listed too");
    let a = list.iter().find(|r| r.id == admin).unwrap();
    assert_eq!((a.origin.as_str(), a.state.as_str(), a.scope_key.as_str()), ("admin", "active", "a.example"));
    assert_eq!(a.sites.len(), 2);
    let on_a = a.sites.iter().find(|s| s.domain == "a.example").unwrap();
    assert_eq!((on_a.approvals, on_a.uses, on_a.state.as_str()), (2, 2, "active"));
    assert!(on_a.last_ok_at.is_some() && on_a.rank > 0.5);
    let on_z = a.sites.iter().find(|s| s.domain == "z.example").unwrap();
    assert_eq!((on_z.rejections, on_z.state.as_str(), on_z.suspended_reason.as_deref()), (1, "suspended", Some("rejected")));
    let retired = list.iter().find(|r| r.id == gone).unwrap();
    assert_eq!((retired.state.as_str(), retired.retired_reason.as_deref()), ("retired", Some("admin")));
    assert!(list.iter().any(|r| r.origin == "shipped"));

    let export = repo.export_recipes()?;
    let keys: Vec<&str> = export.iter().map(|e| e.scope_key.as_str()).collect();
    assert_eq!(keys, ["a.example", "b.example"], "admin and model recipes, live only; no shipped one");
    assert!(export.iter().all(|e| e.scope_kind == "site" && e.recipe_json.contains("dsl_version")));
    let _ = llm;
    Ok(())
}

#[test]
fn an_admin_suspension_stops_the_recipe_on_that_site_only_and_two_canary_passes_lift_it() -> Result<()> {
    let (_d, repo) = rig();
    let id = id_of(repo.insert_recipe(&new_recipe("a.example", 1, Origin::Llm), t0())?);
    repo.recipe_event(id, "a.example", Event::Approval, t0())?;
    repo.recipe_event(id, "b.example", Event::Approval, t0())?;

    assert!(repo.admin_suspend_recipe(id, "A.example ", t0())?);
    let offered = |d: &str| -> Result<bool> { Ok(repo.recipes_for_site(d, None, t0())?.iter().any(|r| r.id == id)) };
    assert!(!offered("a.example")?, "suspended where the admin said");
    assert!(repo.recipes_for_canary("a.example", None, t0())?.iter().any(|r| r.id == id), "a canary may still try it");
    let list = repo.list_recipes(t0())?;
    let on = |d: &str| list[0].sites.iter().find(|s| s.domain == d).unwrap().clone();
    assert_eq!((on("a.example").state.as_str(), on("a.example").suspended_reason.as_deref()), ("suspended", Some("failing")));
    assert_eq!(on("b.example").state, "active", "the other site is untouched");

    repo.recipe_event(id, "a.example", Event::CanaryPass, t0())?;
    assert!(!offered("a.example")?, "one pass is not enough");
    repo.recipe_event(id, "a.example", Event::CanaryPass, t0())?;
    assert!(offered("a.example")?, "two passes in a row lift it");

    assert!(!repo.admin_suspend_recipe(9999, "a.example", t0())?, "no such recipe");
    assert!(repo.admin_suspend_recipe(id, "", t0()).is_err());
    Ok(())
}

#[test]
fn an_admin_suspension_does_not_overwrite_mcrs_rejection() -> Result<()> {
    let (_d, repo) = rig();
    let id = id_of(repo.insert_recipe(&new_recipe("a.example", 1, Origin::Admin), t0())?);
    repo.recipe_event(id, "a.example", Event::Rejection, t0())?;
    assert!(repo.admin_suspend_recipe(id, "a.example", t0() + Duration::hours(1))?);
    repo.recipe_event(id, "a.example", Event::CanaryPass, t0())?;
    repo.recipe_event(id, "a.example", Event::CanaryPass, t0())?;
    let list = repo.list_recipes(t0())?;
    assert_eq!(list[0].sites[0].state, "suspended", "canary passes cannot lift a rejection");
    assert_eq!(list[0].sites[0].suspended_reason.as_deref(), Some("rejected"));
    Ok(())
}

#[test]
fn an_admin_retirement_refuses_the_same_recipe_later_and_a_shipped_one_cannot_be_retired() -> Result<()> {
    let (dir, repo) = rig();
    let new = new_recipe("a.example", 1, Origin::Llm);
    let id = id_of(repo.insert_recipe(&new, t0())?);
    assert_eq!(repo.admin_retire_recipe(id, t0())?, AdminRetire::Retired);
    let (state, reason): (String, Option<String>) = raw(&dir)
        .query_row("SELECT state, retired_reason FROM recipes WHERE id = ?", params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    assert_eq!((state.as_str(), reason.as_deref()), ("retired", Some(RetiredReason::Admin.as_str())));
    assert_eq!(repo.insert_recipe(&new, t0())?, InsertOutcome::Refused("admin".into()), "the model cannot bring it back");
    let mut as_admin = new.clone();
    as_admin.origin = Origin::Admin;
    assert_eq!(repo.insert_recipe(&as_admin, t0())?, InsertOutcome::Refused("admin".into()), "nor an import");
    assert_eq!(repo.admin_retire_recipe(id, t0())?, AdminRetire::AlreadyRetired);
    assert_eq!(repo.admin_retire_recipe(4242, t0())?, AdminRetire::NotFound);

    let j = json(900);
    repo.sync_shipped(&[ShippedRecipe { name: "generic-x".into(), hash: sha("gen"), steps_hash: sha(&j), recipe_json: j, dsl_version: 1 }], t0())?;
    let shipped = repo.list_recipes(t0())?.into_iter().find(|r| r.origin == "shipped").unwrap().id;
    assert_eq!(repo.admin_retire_recipe(shipped, t0())?, AdminRetire::Shipped);
    assert!(repo.list_recipes(t0())?.iter().any(|r| r.id == shipped && r.state != "retired"), "still there");
    Ok(())
}

// ----------------------------------------------------------------- report

struct Seed<'a> {
    dir: &'a TempDir,
    repo: &'a Repository,
    n: u32,
}

impl Seed<'_> {
    /// One finished attempt on its own job.
    fn attempt(&mut self, domain: Option<&str>, mode: &str, end: AttemptEnd, at: DateTime<Utc>) -> (i64, i64) {
        self.n += 1;
        let job = self
            .repo
            .enqueue(&NewJob::new(&format!("https://news.example/story-{}", self.n), &format!("{}_ANNA_TEST", self.n), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();
        let id = self.repo.start_attempt(job, mode, at).unwrap();
        let end = AttemptEnd { domain: domain.map(str::to_string), ..end };
        self.repo.finish_attempt(id, &end, at).unwrap();
        (job, id)
    }

    fn exec(&self, id: i64, sql: &str) {
        raw(self.dir).execute(sql, params![id]).unwrap();
    }
}

fn held(expected: Option<f64>, ms: i64) -> AttemptEnd {
    AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        held_url: Some("https://cdn.example/v.mp4".into()),
        expected_secs: expected,
        duration_ms: Some(ms),
        llm_calls: 1,
        tokens_in: 100,
        tokens_out: 20,
        ..Default::default()
    }
}

/// The reason the desk's rejection stores with the decision.
fn rejected_for(dir: &TempDir, attempt: i64, reason: &str) {
    raw(dir).execute("UPDATE repair_attempts SET mcr_reason = ? WHERE id = ?", params![reason, attempt]).unwrap();
}

#[test]
fn the_repair_report_counts_per_site_and_leaves_rates_blank_when_nothing_can_be_measured() -> Result<()> {
    let (dir, repo) = rig();
    let now = Utc::now();
    let mut s = Seed { dir: &dir, repo: &repo, n: 0 };

    // a.example: three held (two with a stated length), one shadow, one nothing, one «next choice»
    let (_, a1) = s.attempt(Some("a.example"), "hold", held(Some(60.0), 1000), now - Duration::days(2));
    let (j2, a2) = s.attempt(Some("a.example"), "hold", held(Some(180.0), 3000), now - Duration::days(3));
    let (_, a3) = s.attempt(Some("a.example"), "hold", held(None, 5000), now - Duration::days(4));
    let shadow = AttemptEnd { outcome: Some("shadow_found".into()), expected_secs: Some(60.0), duration_ms: Some(7000), ..Default::default() };
    s.attempt(Some("a.example"), "shadow", shadow, now - Duration::days(5));
    let none = AttemptEnd { outcome: Some("nothing_verified".into()), duration_ms: Some(9000), ..Default::default() };
    s.attempt(Some("a.example"), "hold", none, now - Duration::days(6));
    s.exec(a1, "UPDATE repair_attempts SET mcr_decision = 'approved', image_distance = 4 WHERE id = ?");
    s.exec(a2, "UPDATE repair_attempts SET mcr_decision = 'rejected', image_distance = 30 WHERE id = ?");
    s.exec(a3, "UPDATE repair_attempts SET mcr_decision = 'expired' WHERE id = ?");
    rejected_for(&dir, a2, "wrong_video");
    // a2's job went on to MCR's «next choice»: it ended held, took 7777 ms and was approved
    raw(&dir).execute(
        "INSERT INTO repair_attempts (job_id, started_at, mode, rung, outcome, domain, duration_ms, mcr_decision, expected_secs, image_distance)
         VALUES (?, ?, 'hold', 'next', 'held', 'a.example', 7777, 'approved', 60.0, 2)",
        params![j2, omni_core::timestamps::format(now - Duration::days(3) + Duration::hours(1))],
    )?;

    // b.example: a rejection for another reason, a hold with no length and no picture
    let (_, b1) = s.attempt(Some("b.example"), "hold", held(None, 2000), now - Duration::days(1));
    s.exec(b1, "UPDATE repair_attempts SET mcr_decision = 'rejected' WHERE id = ?");
    rejected_for(&dir, b1, "other");

    // no domain, and one too old for the period
    s.attempt(None, "hold", AttemptEnd { outcome: Some("no_domain".into()), ..Default::default() }, now - Duration::days(1));
    s.attempt(Some("old.example"), "hold", held(Some(60.0), 1), now - Duration::days(40));

    repo.record_selfheal("a.example", "found", None, None, 2, None, now - Duration::days(2))?;
    repo.record_selfheal("a.example", "nothing", None, None, 2, None, now - Duration::days(3))?;
    repo.record_selfheal("a.example", "skipped", None, None, 0, None, now - Duration::days(3))?;
    repo.record_selfheal("heal-only.example", "found", None, None, 1, None, now - Duration::days(1))?;

    let report = repo.repair_report(now - Duration::days(30))?;
    let by = |d: &str| report.iter().find(|r| r.domain == d).unwrap_or_else(|| panic!("no row for {d}: {report:?}")).clone();
    assert!(report.iter().all(|r| r.domain != "old.example"), "outside the period");

    let a = by("a.example");
    assert_eq!((a.attempts, a.next_choices, a.shadow_attempts, a.hold_attempts), (5, 1, 1, 4), "a «next choice» is not a ladder run");
    assert_eq!(a.found, 4, "nor is it a find");
    assert_eq!(a.held, 4, "but it was held");
    assert_eq!((a.approved, a.rejected_wrong_video, a.rejected_other, a.expired), (2, 1, 0, 1), "its decision still counts");
    assert_eq!(a.length_stated, 3, "a1, a2 and the shadow run: the page stated a length");
    assert_eq!(a.length_stated_rate(), Some(0.75));
    assert_eq!((a.image_known, a.image_matched), (2, 1), "the next choice's picture is not a find's");
    assert_eq!(a.image_match_rate(), Some(0.5));
    assert_eq!((a.llm_calls, a.tokens_in, a.tokens_out), (3, 300, 60));
    assert_eq!(a.median_duration_ms, Some(5000), "1000 3000 5000 7000 9000; the next choice's 7777 is not a run");
    assert_eq!((a.heal_runs, a.heal_found), (2, 1), "skipped runs are not runs");

    let b = by("b.example");
    assert_eq!((b.attempts, b.next_choices, b.found, b.rejected_wrong_video, b.rejected_other), (1, 0, 1, 0, 1));
    assert_eq!((b.length_stated, b.image_known), (0, 0));
    assert_eq!((b.length_stated_rate(), b.image_match_rate()), (Some(0.0), None), "no picture to compare is blank, not 0 %");

    let unknown = by("(unknown)");
    assert_eq!((unknown.attempts, unknown.found, unknown.median_duration_ms, unknown.length_stated_rate()), (1, 0, None, None));
    let heal_only = by("heal-only.example");
    assert_eq!((heal_only.attempts, heal_only.heal_runs, heal_only.heal_found), (0, 1, 1));
    assert_eq!(report[0].domain, "a.example", "busiest first");

    assert!(repo.repair_report(now + Duration::days(1))?.is_empty(), "an empty period is an empty report");
    Ok(())
}

#[test]
fn each_rejection_keeps_its_own_reason_even_after_a_retry() -> Result<()> {
    let (dir, repo) = rig();
    let now = Utc::now();
    let mut s = Seed { dir: &dir, repo: &repo, n: 0 };
    let (job, first) = s.attempt(Some("a.example"), "hold", held(None, 1000), now - Duration::days(2));
    s.exec(first, "UPDATE repair_attempts SET mcr_decision = 'rejected' WHERE id = ?");
    rejected_for(&dir, first, "other");
    // MCR retried the job; the new hold was rejected as the wrong video, which is what the held row now says
    let second = repo.start_attempt(job, "hold", now - Duration::days(1))?;
    repo.finish_attempt(second, &AttemptEnd { domain: Some("a.example".into()), ..held(None, 1000) }, now - Duration::days(1))?;
    s.exec(second, "UPDATE repair_attempts SET mcr_decision = 'rejected' WHERE id = ?");
    rejected_for(&dir, second, "wrong_video");
    raw(&dir).execute(
        "INSERT INTO held_outputs (job_id, path, bytes, sha256, created_at, expires_at, decision, reason)
         VALUES (?, 'x', 1, 'x', 'x', 'x', 'rejected', 'wrong_video')",
        params![job],
    )?;

    // A row from before the reason was stored has none: it counts as another reason.
    let old = repo.start_attempt(job, "hold", now - Duration::hours(5))?;
    repo.finish_attempt(old, &AttemptEnd { domain: Some("a.example".into()), ..held(None, 1000) }, now - Duration::hours(5))?;
    s.exec(old, "UPDATE repair_attempts SET mcr_decision = 'rejected' WHERE id = ?");

    let report = repo.repair_report(now - Duration::days(30))?;
    let a = report.iter().find(|r| r.domain == "a.example").unwrap();
    assert_eq!((a.rejected_wrong_video, a.rejected_other), (1, 2), "{a:?}");
    Ok(())
}

// ------------------------------------------------- retire, export, import

#[test]
fn retiring_a_recipe_that_was_only_evicted_makes_it_unreturnable() -> Result<()> {
    let (_d, repo) = rig();
    for n in 0..5 {
        repo.insert_recipe(&new_recipe("a.example", n, Origin::Llm), t0() + Duration::minutes(n as i64))?;
    }
    let evicted = repo.list_recipes(t0())?.into_iter().find(|r| r.state == "retired").expect("the scope holds 4: one was evicted");
    assert_eq!(evicted.retired_reason.as_deref(), Some("evicted"));
    let n = (0..5).find(|n| new_recipe("a.example", *n, Origin::Llm).hash == evicted.hash).unwrap();

    assert_eq!(repo.admin_retire_recipe(evicted.id, t0())?, AdminRetire::Retired, "not «already retired»: it could still come back");
    assert_eq!(repo.admin_retire_recipe(evicted.id, t0())?, AdminRetire::AlreadyRetired, "now it cannot");
    let again = repo.insert_recipe(&new_recipe("a.example", n, Origin::Admin), t0())?;
    assert_eq!(again, InsertOutcome::Refused("admin".into()));
    Ok(())
}

#[test]
fn the_export_leaves_out_a_recipe_whose_steps_are_barred_on_its_own_site() -> Result<()> {
    let (_d, repo) = rig();
    let bad = id_of(repo.insert_recipe(&new_recipe("a.example", 1, Origin::Admin), t0())?);
    repo.insert_recipe(&new_recipe("a.example", 2, Origin::Admin), t0())?;
    repo.insert_recipe(&new_recipe("b.example", 1, Origin::Admin), t0())?;
    assert_eq!(repo.export_recipes()?.len(), 3);
    repo.recipe_event(bad, "a.example", Event::Rejection, t0())?; // MCR: wrong video; its steps are barred on a.example
    let keys: Vec<(String, String)> = repo.export_recipes()?.into_iter().map(|e| (e.scope_key, e.recipe_json)).collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.iter().any(|(k, j)| k == "b.example" && j.contains("\"f1\"")), "the same steps on another site are not barred there");
    assert!(!keys.iter().any(|(k, j)| k == "a.example" && j.contains("\"f1\"")), "the barred recipe is gone from the file");
    Ok(())
}

#[test]
fn an_import_names_what_it_evicted_and_never_evicts_its_own_entries() -> Result<()> {
    let (_d, repo) = rig();
    let llm: Vec<i64> = (0..4).map(|n| id_of(repo.insert_recipe(&new_recipe("x.example", n, Origin::Llm), t0()).unwrap())).collect();
    let batch: Vec<NewRecipe> = (10..12).map(|n| new_recipe("x.example", n, Origin::Admin)).collect();
    let out = repo.import_recipes(&batch, t0() + Duration::days(1))?;
    assert!(matches!(out[0].outcome, InsertOutcome::Inserted(_)) && matches!(out[1].outcome, InsertOutcome::Inserted(_)));
    assert_eq!((out[0].evicted.len(), out[1].evicted.len()), (1, 1), "each entry pushed one model recipe out: {out:?}");
    for e in out.iter().flat_map(|r| &r.evicted) {
        assert!(llm.contains(&e.id), "only a model recipe is named");
        assert_eq!((e.scope_kind.as_str(), e.scope_key.as_str(), e.why.as_str()), ("site", "x.example", "its scope was full"));
    }
    let live: Vec<_> = repo.list_recipes(t0())?.into_iter().filter(|r| r.state != "retired").collect();
    assert_eq!(live.iter().filter(|r| r.origin == "admin").count(), 2, "both imported entries are alive");

    // Five entries for one scope: the fifth finds only its own batch to evict, so it is not stored.
    let (_d2, repo2) = rig();
    let five: Vec<NewRecipe> = (0..5).map(|n| new_recipe("y.example", n, Origin::Admin)).collect();
    let out = repo2.import_recipes(&five, t0())?;
    assert!(out[..4].iter().all(|r| matches!(r.outcome, InsertOutcome::Inserted(_))));
    assert_eq!(out[4].outcome, InsertOutcome::OverCap);
    assert!(out[4].evicted.is_empty());
    assert_eq!(repo2.list_recipes(t0())?.iter().filter(|r| r.state != "retired").count(), 4, "none of the first four was evicted");
    Ok(())
}

#[test]
fn an_error_in_the_middle_of_an_import_stores_nothing() -> Result<()> {
    let (_d, repo) = rig();
    let mut bad = new_recipe("c.example", 3, Origin::Admin);
    bad.hash.clear(); // the store refuses a recipe with no hash
    let batch = vec![new_recipe("a.example", 1, Origin::Admin), new_recipe("b.example", 2, Origin::Admin), bad];
    assert!(repo.import_recipes(&batch, t0()).is_err());
    assert!(repo.list_recipes(t0())?.is_empty(), "the two good entries were rolled back");
    Ok(())
}

#[test]
fn retiring_an_already_retired_recipe_bars_nothing_so_the_player_recipe_that_replaced_it_still_works() -> Result<()> {
    let (_d, repo) = rig();
    let steps = sha(&json(1));
    let mk = |kind: ScopeKind, key: &str| NewRecipe {
        scope_kind: kind,
        scope_key: key.to_string(),
        family: Some("jwplayer".into()),
        hash: sha(&format!("{}|{key}|{}", kind.as_str(), json(1))),
        steps_hash: steps.clone(),
        recipe_json: json(1),
        dsl_version: 1,
        origin: Origin::Llm,
    };
    let a = id_of(repo.insert_recipe(&mk(ScopeKind::Site, "a.example"), t0())?);
    let b = id_of(repo.insert_recipe(&mk(ScopeKind::Site, "b.example"), t0())?);
    for id in [a, b] {
        let d = if id == a { "a.example" } else { "b.example" };
        repo.recipe_event(id, d, Event::Approval, t0())?;
        repo.recipe_event(id, d, Event::Approval, t0())?;
    }
    let player = mk(ScopeKind::Player, "jwplayer");
    let pid = repo.promote_to_player(&steps, "jwplayer", &player.hash, &player.recipe_json, 1, t0())?.expect("two sites proved the steps");
    let source = repo.list_recipes(t0())?.into_iter().find(|r| r.id == a).unwrap();
    assert_eq!(source.retired_reason.as_deref(), Some("superseded"));
    let offered = || -> Result<bool> { Ok(repo.recipes_for_site("a.example", Some("jwplayer"), t0())?.iter().any(|r| r.id == pid)) };
    assert!(offered()?, "the player recipe works on a.example");

    assert_eq!(repo.admin_retire_recipe(a, t0())?, AdminRetire::Retired);
    assert!(!repo.recipe_steps_barred(&steps, "a.example")?, "no bar was written");
    assert!(offered()?, "and the player recipe is still offered there");
    let source = repo.list_recipes(t0())?.into_iter().find(|r| r.id == a).unwrap();
    assert_eq!(source.retired_reason.as_deref(), Some("admin"));
    assert_eq!(repo.insert_recipe(&mk(ScopeKind::Site, "a.example"), t0())?, InsertOutcome::Refused("admin".into()), "yet the source cannot return");

    // Retiring a live recipe still bars its steps on its own site.
    let live = id_of(repo.insert_recipe(&new_recipe("c.example", 7, Origin::Llm), t0())?);
    let live_steps = new_recipe("c.example", 7, Origin::Llm).steps_hash;
    assert_eq!(repo.admin_retire_recipe(live, t0())?, AdminRetire::Retired);
    assert!(repo.recipe_steps_barred(&live_steps, "c.example")?);
    Ok(())
}
