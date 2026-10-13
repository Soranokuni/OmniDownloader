//! The recipe store (plan P10.10): bounded, scored, self-cleaning.
//!
//! Real SQLite files, explicit `now` values, invented sites and recipes.

use anyhow::Result;
use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use omni_core::recipe_score::{
    Event, InsertOutcome, NewRecipe, Origin, RecipeState, RetiredReason, ScopeKind, ShippedRecipe, SiteState,
    BAR_CAP, MAX_RECIPE_JSON, NONSHIPPED_LIVE_CAP, NONSHIPPED_ROW_CAP, SITE_ROW_CAP, TOMBSTONE_CAP,
};
use omni_core::repository::Repository;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap()
}

fn days(n: i64) -> Duration {
    Duration::days(n)
}

fn repo(dir: &TempDir) -> Repository {
    Repository::new(dir.path().join("omni.db")).unwrap()
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

fn recipe(kind: ScopeKind, key: &str, family: Option<&str>, n: usize, origin: Origin) -> NewRecipe {
    let j = json(n);
    NewRecipe {
        scope_kind: kind,
        scope_key: key.to_string(),
        family: family.map(str::to_string),
        hash: sha(&format!("{}|{key}|{j}", kind.as_str())),
        steps_hash: sha(&j),
        recipe_json: j,
        dsl_version: 1,
        origin,
    }
}

fn site(key: &str, n: usize, origin: Origin) -> NewRecipe {
    recipe(ScopeKind::Site, key, None, n, origin)
}

fn id_of(o: InsertOutcome) -> i64 {
    match o {
        InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) | InsertOutcome::Revived(id) => id,
        other => panic!("expected an id, got {other:?}"),
    }
}

fn state_of(c: &Connection, id: i64) -> (String, Option<String>) {
    c.query_row("SELECT state, retired_reason FROM recipes WHERE id = ?", params![id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn shipped(name: &str, n: usize) -> ShippedRecipe {
    let j = json(1000 + n);
    ShippedRecipe { name: name.to_string(), hash: sha(&format!("generic|{name}|{j}")), steps_hash: sha(&j), recipe_json: j, dsl_version: 1 }
}

#[test]
fn the_same_recipe_merges_and_fills_a_missing_family() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let id = id_of(r.insert_recipe(&site("a.example", 1, Origin::Llm), t0())?);
    assert_eq!(r.insert_recipe(&site("a.example", 1, Origin::Llm), t0())?, InsertOutcome::Merged(id));
    assert_eq!(r.recipe_counts()?.live, 1, "a merge must not add a row");

    let c = raw(&dir);
    let fam: Option<String> = c.query_row("SELECT family FROM recipes WHERE id = ?", params![id], |r| r.get(0))?;
    assert_eq!(fam, None);
    let with_family = recipe(ScopeKind::Site, "a.example", Some("jwplayer"), 1, Origin::Llm);
    assert_eq!(r.insert_recipe(&with_family, t0())?, InsertOutcome::Merged(id));
    let fam: Option<String> = c.query_row("SELECT family FROM recipes WHERE id = ?", params![id], |r| r.get(0))?;
    assert_eq!(fam.as_deref(), Some("jwplayer"));
    // a family once known is not overwritten
    let other = recipe(ScopeKind::Site, "a.example", Some("videojs"), 1, Origin::Llm);
    r.insert_recipe(&other, t0())?;
    let fam: Option<String> = c.query_row("SELECT family FROM recipes WHERE id = ?", params![id], |r| r.get(0))?;
    assert_eq!(fam.as_deref(), Some("jwplayer"));
    assert_eq!(r.recipe_counts()?.live, 1);
    Ok(())
}

#[test]
fn invalid_inserts_are_errors_and_leave_nothing() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let mut big = site("a.example", 1, Origin::Llm);
    big.recipe_json = "x".repeat(MAX_RECIPE_JSON + 1);
    assert!(r.insert_recipe(&big, t0()).is_err());
    big.recipe_json = "x".repeat(MAX_RECIPE_JSON);
    assert!(matches!(r.insert_recipe(&big, t0())?, InsertOutcome::Inserted(_)), "exactly the limit is allowed");
    assert!(r.insert_recipe(&site("b.example", 2, Origin::Shipped), t0()).is_err());
    assert!(r.insert_recipe(&recipe(ScopeKind::Generic, "x", None, 3, Origin::Llm), t0()).is_err());
    assert_eq!(r.recipe_counts()?.live, 1);
    assert_eq!(r.recipe_counts()?.shipped, 0);
    Ok(())
}

#[test]
fn a_rejected_recipe_cannot_come_back_but_an_idle_one_can() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);

    // a tombstone for a hash that is not in the store
    let ghost = site("a.example", 1, Origin::Llm);
    r.tombstone_recipe(&ghost.hash, RetiredReason::Rejected, t0())?;
    assert_eq!(r.insert_recipe(&ghost, t0())?, InsertOutcome::Refused("rejected".into()));
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes"), 0, "a refused insert must leave no row");

    // an admin tombstone refuses as well
    let adm = site("a.example", 2, Origin::Admin);
    r.tombstone_recipe(&adm.hash, RetiredReason::Admin, t0())?;
    assert_eq!(r.insert_recipe(&adm, t0())?, InsertOutcome::Refused("admin".into()));

    // an idle tombstone is stale: it is deleted and the insert goes through
    let old = site("a.example", 3, Origin::Llm);
    r.tombstone_recipe(&old.hash, RetiredReason::Idle, t0())?;
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones WHERE reason = 'idle'"), 1);
    assert!(matches!(r.insert_recipe(&old, t0())?, InsertOutcome::Inserted(_)));
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipe_tombstones WHERE hash = '{}'", old.hash)), 0);

    // a stored recipe retired as rejected refuses too, with no new row
    let live = site("b.example", 4, Origin::Llm);
    let id = id_of(r.insert_recipe(&live, t0())?);
    r.tombstone_recipe(&live.hash, RetiredReason::Rejected, t0() + days(1))?;
    assert_eq!(state_of(&c, id), ("retired".into(), Some("rejected".into())));
    let before = count(&c, "SELECT COUNT(*) FROM recipes");
    assert_eq!(r.insert_recipe(&live, t0() + days(2))?, InsertOutcome::Refused("rejected".into()));
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes"), before);
    assert_eq!(state_of(&c, id).0, "retired");
    Ok(())
}

#[test]
fn an_idle_retired_recipe_is_revived_with_its_scores_and_a_clean_failing_state() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let new = site("a.example", 1, Origin::Llm);
    let id = id_of(r.insert_recipe(&new, t0())?);
    for _ in 0..3 {
        r.recipe_event(id, "a.example", Event::CanaryFail, t0())?;
    }
    r.recipe_event(id, "b.example", Event::Rejection, t0())?;
    assert_eq!(state_of(&c, id).0, "suspended");

    let rep = r.recipe_gc(t0() + days(100))?;
    assert_eq!(rep.retired_idle, 1);
    assert_eq!(state_of(&c, id), ("retired".into(), Some("idle".into())));

    assert_eq!(r.insert_recipe(&new, t0() + days(101))?, InsertOutcome::Revived(id));
    assert_eq!(state_of(&c, id), ("candidate".into(), None));
    let (retired_at, ): (Option<String>,) = c.query_row("SELECT retired_at FROM recipes WHERE id = ?", params![id], |r| Ok((r.get(0)?,)))?;
    assert_eq!(retired_at, None);
    // the failing suspension is lifted, the numbers are kept
    let (state, reason, fails, beta): (String, Option<String>, i64, f64) = c.query_row(
        "SELECT state, suspended_reason, consecutive_fail, beta FROM recipe_sites WHERE recipe_id = ? AND domain = 'a.example'",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    assert_eq!((state.as_str(), reason, fails), ("candidate", None, 0));
    assert!(beta > 3.9, "numbers kept, got {beta}");
    // a rejection is not lifted
    let (state, reason): (String, Option<String>) = c.query_row(
        "SELECT state, suspended_reason FROM recipe_sites WHERE recipe_id = ? AND domain = 'b.example'",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((state.as_str(), reason.as_deref()), ("suspended", Some("rejected")));
    Ok(())
}

#[test]
fn the_scope_cap_retires_the_lowest_ranked_llm_recipe() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let ids: Vec<i64> = (1..=4).map(|n| id_of(r.insert_recipe(&site("a.example", n, Origin::Llm), t0()).unwrap())).collect();
    // recipe 1 has the worst record, 3 the best
    r.recipe_event(ids[0], "a.example", Event::CanaryFail, t0())?;
    r.recipe_event(ids[1], "a.example", Event::Approval, t0())?;
    r.recipe_event(ids[2], "a.example", Event::Approval, t0())?;
    r.recipe_event(ids[2], "a.example", Event::Approval, t0())?;
    r.recipe_event(ids[3], "a.example", Event::CanaryPass, t0())?;

    let fifth = id_of(r.insert_recipe(&site("a.example", 5, Origin::Llm), t0() + days(1))?);
    assert_eq!(state_of(&c, ids[0]), ("retired".into(), Some("evicted".into())));
    for i in 1..4 {
        assert_ne!(state_of(&c, ids[i]).0, "retired", "recipe {} must stay", i + 1);
    }
    assert_ne!(state_of(&c, fifth).0, "retired");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE scope_key = 'a.example' AND state <> 'retired'"), 4);
    // another scope is not affected
    r.insert_recipe(&site("b.example", 6, Origin::Llm), t0())?;
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE state = 'retired'"), 1);
    Ok(())
}

#[test]
fn an_llm_insert_never_evicts_admin_recipes_and_admin_evicts_llm_first() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    for n in 1..=4 {
        r.insert_recipe(&site("a.example", n, Origin::Admin), t0())?;
    }
    let rows = count(&c, "SELECT COUNT(*) FROM recipes");
    assert_eq!(r.insert_recipe(&site("a.example", 5, Origin::Llm), t0())?, InsertOutcome::OverCap);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes"), rows, "OverCap must change nothing");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE state = 'retired'"), 0);

    // an admin insert may evict an admin recipe
    assert!(matches!(r.insert_recipe(&site("a.example", 6, Origin::Admin), t0())?, InsertOutcome::Inserted(_)));
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE state = 'retired' AND origin = 'admin'"), 1);

    // an llm recipe goes before an admin one, even with the better record
    let dir2 = TempDir::new()?;
    let r2 = repo(&dir2);
    let c2 = raw(&dir2);
    for n in 1..=3 {
        r2.insert_recipe(&site("a.example", n, Origin::Admin), t0())?;
    }
    let llm = id_of(r2.insert_recipe(&site("a.example", 4, Origin::Llm), t0())?);
    for _ in 0..5 {
        r2.recipe_event(llm, "a.example", Event::Approval, t0())?;
    }
    r2.insert_recipe(&site("a.example", 7, Origin::Admin), t0())?;
    assert_eq!(state_of(&c2, llm), ("retired".into(), Some("evicted".into())));
    assert_eq!(count(&c2, "SELECT COUNT(*) FROM recipes WHERE origin = 'admin' AND state = 'retired'"), 0);
    Ok(())
}

/// 64 sites of 4 recipes fill the live cap exactly.
fn fill_live_cap(r: &Repository, origin: Origin) -> Vec<i64> {
    (0..NONSHIPPED_LIVE_CAP)
        .map(|i| id_of(r.insert_recipe(&site(&format!("s{}.example", i / 4), i, origin), t0()).unwrap()))
        .collect()
}

#[test]
fn the_global_live_cap_evicts_the_lowest_ranked_recipe_anywhere() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let ids = fill_live_cap(&r, Origin::Llm);
    // everyone but one recipe has a good record
    let worst = ids[137];
    for (i, id) in ids.iter().enumerate() {
        if *id != worst {
            r.recipe_event(*id, &format!("s{}.example", i / 4), Event::Approval, t0())?;
        }
    }
    r.recipe_event(worst, "s34.example", Event::CanaryFail, t0())?;
    assert_eq!(r.recipe_counts()?.live, NONSHIPPED_LIVE_CAP);

    let new = id_of(r.insert_recipe(&site("fresh.example", 9999, Origin::Llm), t0() + days(1))?);
    assert_eq!(state_of(&c, worst), ("retired".into(), Some("evicted".into())));
    assert_ne!(state_of(&c, new).0, "retired");
    let counts = r.recipe_counts()?;
    assert_eq!((counts.live, counts.retired), (NONSHIPPED_LIVE_CAP, 1));
    Ok(())
}

#[test]
fn a_store_full_of_admin_recipes_refuses_llm_inserts_without_change() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    fill_live_cap(&r, Origin::Admin);
    let before = r.recipe_counts()?;
    assert_eq!(r.insert_recipe(&site("fresh.example", 9999, Origin::Llm), t0())?, InsertOutcome::OverCap);
    assert_eq!(r.recipe_counts()?, before);
    Ok(())
}

#[test]
fn the_row_cap_deletes_the_oldest_retired_recipe_and_remembers_it() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    fill_live_cap(&r, Origin::Llm);
    let extra = NONSHIPPED_ROW_CAP - NONSHIPPED_LIVE_CAP + 30;
    let mut first_evicted: Option<(i64, String)> = None;
    for k in 0..extra {
        let n = 5000 + k;
        r.insert_recipe(&site(&format!("new{k}.example"), n, Origin::Llm), t0() + Duration::minutes(k as i64))?;
        let rows = count(&c, "SELECT COUNT(*) FROM recipes");
        assert!(rows <= NONSHIPPED_ROW_CAP as i64, "{rows} rows after insert {k}");
        if k == 0 {
            first_evicted = Some(c.query_row(
                "SELECT id, hash FROM recipes WHERE state = 'retired'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?);
        }
    }
    let counts = r.recipe_counts()?;
    assert_eq!(counts.live, NONSHIPPED_LIVE_CAP);
    assert_eq!(counts.live + counts.retired, NONSHIPPED_ROW_CAP, "the row cap is reached and held");
    // the first evicted recipe was the oldest retired one: gone, but remembered
    let (id, hash) = first_evicted.unwrap();
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipes WHERE id = {id}")), 0);
    let reason: String = c.query_row("SELECT reason FROM recipe_tombstones WHERE hash = ?", params![hash], |r| r.get(0))?;
    assert_eq!(reason, "evicted");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones") as usize, extra - (NONSHIPPED_ROW_CAP - NONSHIPPED_LIVE_CAP));
    Ok(())
}

#[test]
fn a_site_insert_merges_into_a_player_recipe_of_the_same_family_and_steps() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let player = id_of(r.insert_recipe(&recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 1, Origin::Llm), t0())?);
    let rows = r.recipe_counts()?.live;

    let same = recipe(ScopeKind::Site, "a.example", Some("jwplayer"), 1, Origin::Llm);
    assert_eq!(r.insert_recipe(&same, t0())?, InsertOutcome::Merged(player));
    let other_site = recipe(ScopeKind::Site, "b.example", Some("jwplayer"), 1, Origin::Llm);
    assert_eq!(r.insert_recipe(&other_site, t0())?, InsertOutcome::Merged(player));
    assert_eq!(r.recipe_counts()?.live, rows, "shared players do not multiply rows");

    // other steps, another family, or an unknown family are separate recipes
    for s in [
        recipe(ScopeKind::Site, "a.example", Some("jwplayer"), 2, Origin::Llm),
        recipe(ScopeKind::Site, "a.example", Some("videojs"), 1, Origin::Llm),
        recipe(ScopeKind::Site, "c.example", None, 1, Origin::Llm),
    ] {
        assert!(matches!(r.insert_recipe(&s, t0())?, InsertOutcome::Inserted(_)), "{s:?}");
    }
    Ok(())
}

#[test]
fn recipes_for_site_orders_by_tier_and_leaves_out_what_must_not_run() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let now = t0() + days(1);
    let mk = |k: ScopeKind, key: &str, fam: Option<&str>, n: usize| id_of(r.insert_recipe(&recipe(k, key, fam, n, Origin::Llm), t0()).unwrap());

    let s_best = mk(ScopeKind::Site, "a.example", None, 1);
    let s_ok = mk(ScopeKind::Site, "a.example", None, 2);
    let s_new = mk(ScopeKind::Site, "a.example", None, 3);
    let s_bad = mk(ScopeKind::Site, "a.example", None, 4);
    let s_other = mk(ScopeKind::Site, "b.example", None, 5);
    let p_here = mk(ScopeKind::Player, "jwplayer", Some("jwplayer"), 6);
    let p_pooled = mk(ScopeKind::Player, "jwplayer", Some("jwplayer"), 7);
    let p_other = mk(ScopeKind::Player, "videojs", Some("videojs"), 8);
    let p_retired = mk(ScopeKind::Player, "jwplayer", Some("jwplayer"), 9);
    r.sync_shipped(&[shipped("g_fine", 1), shipped("g_bad", 2)], t0())?;
    let generic = r.recipes_for_site("zzz.example", None, now)?;
    let g_fine = generic.iter().find(|x| x.scope_key == "g_fine").unwrap().id;
    let g_bad = generic.iter().find(|x| x.scope_key == "g_bad").unwrap().id;

    r.recipe_event(s_best, "a.example", Event::Approval, t0())?;
    r.recipe_event(s_best, "a.example", Event::Approval, t0())?;
    r.recipe_event(s_ok, "a.example", Event::Approval, t0())?;
    r.recipe_event(s_bad, "a.example", Event::Rejection, t0())?;
    r.recipe_event(s_other, "b.example", Event::Approval, t0())?;
    r.recipe_event(p_here, "a.example", Event::Approval, t0())?;
    r.recipe_event(p_pooled, "c.example", Event::Approval, t0())?;
    r.recipe_event(p_other, "a.example", Event::Approval, t0())?;
    r.recipe_event(g_bad, "a.example", Event::Rejection, t0())?;
    r.tombstone_recipe(&recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 9, Origin::Llm).hash, RetiredReason::Idle, t0())?;
    let _ = (p_retired, s_new);

    let got = r.recipes_for_site("a.example", Some("jwplayer"), now)?;
    let order: Vec<i64> = got.iter().map(|x| x.id).collect();
    assert_eq!(order, vec![s_best, s_ok, s_new, p_here, p_pooled, g_fine], "{got:#?}");
    assert_eq!(got[0].site_state, Some(SiteState::Active));
    assert_eq!(got[2].site_state, None);
    assert!(got[0].rank > got[1].rank, "two approvals outrank one");
    assert_eq!(got[0].origin, Origin::Llm);
    assert_eq!(got[5].origin, Origin::Shipped);

    // without a family no player recipe is offered
    let order: Vec<i64> = r.recipes_for_site("a.example", None, now)?.iter().map(|x| x.id).collect();
    assert_eq!(order, vec![s_best, s_ok, s_new, g_fine]);
    // another domain sees its own recipes and the generic ones, not a.example's
    let order: Vec<i64> = r.recipes_for_site("b.example", Some("videojs"), now)?.iter().map(|x| x.id).collect();
    assert_eq!(order[..2], [s_other, p_other]);
    // untried generic recipes tie on rank: hash order decides
    let hash_of = |id: i64| r.recipes_for_site("b.example", Some("videojs"), now).unwrap().into_iter().find(|x| x.id == id).unwrap().hash;
    let mut generics = vec![g_fine, g_bad];
    generics.sort_by_key(|id| hash_of(*id));
    assert_eq!(order[2..], generics[..]);
    Ok(())
}

#[test]
fn an_event_creates_the_row_and_the_recipe_state_follows_it() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let id = id_of(r.insert_recipe(&site("a.example", 1, Origin::Llm), t0())?);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites"), 0);
    assert_eq!(state_of(&c, id).0, "candidate");

    let row = r.recipe_event(id, "a.example", Event::Approval, t0() + days(1))?;
    assert_eq!((row.alpha, row.state), (3.0, SiteState::Active));
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites"), 1);
    assert_eq!(state_of(&c, id).0, "active");

    r.recipe_event(id, "a.example", Event::Rejection, t0() + days(2))?;
    assert_eq!(state_of(&c, id).0, "suspended");
    // a second domain on its prior keeps the recipe a candidate
    r.recipe_event(id, "b.example", Event::CanaryPass, t0() + days(2))?;
    assert_eq!(state_of(&c, id).0, "candidate");

    assert!(r.recipe_event(99_999, "a.example", Event::Approval, t0()).is_err());
    assert!(r.recipe_event(id, "", Event::Approval, t0()).is_err());

    // an event on a retired recipe updates its row; the recipe stays retired
    r.tombstone_recipe(&site("a.example", 1, Origin::Llm).hash, RetiredReason::Idle, t0() + days(3))?;
    r.recipe_event(id, "c.example", Event::Approval, t0() + days(4))?;
    assert_eq!(state_of(&c, id), ("retired".into(), Some("idle".into())));
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'c.example'"), 1);
    Ok(())
}

#[test]
fn promotion_moves_the_scores_retires_the_sources_and_later_inserts_merge() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let a = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "a.example", Some("jwplayer"), 1, Origin::Llm), t0())?);
    let b = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "b.example", Some("jwplayer"), 1, Origin::Admin), t0())?);
    let steps = sha(&json(1));
    let player = recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 1, Origin::Llm);

    // one proven site is not enough, and nothing changes
    r.recipe_event(a, "a.example", Event::Approval, t0())?;
    assert_eq!(r.promote_to_player(&steps, "jwplayer", &player.hash, &player.recipe_json, 1, t0())?, None);
    assert_eq!(state_of(&c, a).0, "active");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE scope_kind = 'player'"), 0);

    r.recipe_event(b, "b.example", Event::Approval, t0())?;
    r.recipe_event(b, "b.example", Event::Approval, t0())?;
    // a different family does not promote
    assert_eq!(r.promote_to_player(&steps, "videojs", &player.hash, &player.recipe_json, 1, t0())?, None);

    let pid = r
        .promote_to_player(&steps, "jwplayer", &player.hash, &player.recipe_json, 1, t0())?
        .expect("promoted");
    assert_eq!(state_of(&c, a), ("retired".into(), Some("superseded".into())));
    assert_eq!(state_of(&c, b), ("retired".into(), Some("superseded".into())));
    assert_eq!(state_of(&c, pid).0, "active");
    let (origin, kind, key): (String, String, String) =
        c.query_row("SELECT origin, scope_kind, scope_key FROM recipes WHERE id = ?", params![pid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    assert_eq!((origin.as_str(), kind.as_str(), key.as_str()), ("admin", "player", "jwplayer"), "admin if any source is admin");
    // the scores moved: a.example alpha 3 (one approval), b.example alpha 5 (two)
    let alpha = |dom: &str| -> f64 {
        c.query_row("SELECT alpha FROM recipe_sites WHERE recipe_id = ? AND domain = ?", params![pid, dom], |r| r.get(0)).unwrap()
    };
    assert_eq!((alpha("a.example"), alpha("b.example")), (3.0, 5.0));
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipe_sites WHERE recipe_id IN ({a}, {b})")), 0);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites"), 2);

    // a third site with these steps now merges into the player recipe
    let third = recipe(ScopeKind::Site, "c.example", Some("jwplayer"), 1, Origin::Llm);
    assert_eq!(r.insert_recipe(&third, t0() + days(1))?, InsertOutcome::Merged(pid));
    // and the player recipe is offered to a site of the family
    let ids: Vec<i64> = r.recipes_for_site("c.example", Some("jwplayer"), t0() + days(1))?.iter().map(|x| x.id).collect();
    assert_eq!(ids, vec![pid]);
    Ok(())
}

#[test]
fn promotion_adds_up_a_domain_both_the_player_and_a_source_have() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let a = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "a.example", Some("jwplayer"), 1, Origin::Llm), t0())?);
    let b = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "b.example", Some("jwplayer"), 1, Origin::Llm), t0())?);
    // the player recipe already exists (inserted directly) and has a row for a.example
    let player = recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 1, Origin::Llm);
    let pid = id_of(r.insert_recipe(&player, t0())?);
    r.recipe_event(pid, "a.example", Event::Approval, t0())?;
    r.recipe_event(a, "a.example", Event::Approval, t0())?;
    r.recipe_event(a, "a.example", Event::Approval, t0())?;
    r.recipe_event(b, "b.example", Event::Approval, t0())?;

    let got = r.promote_to_player(&sha(&json(1)), "jwplayer", &player.hash, &player.recipe_json, 1, t0())?;
    assert_eq!(got, Some(pid));
    // 1 + (3-1) + (5-1) = 7
    let (alpha, approvals): (f64, i64) = c.query_row(
        "SELECT alpha, approvals FROM recipe_sites WHERE recipe_id = ? AND domain = 'a.example'",
        params![pid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((alpha, approvals), (7.0, 3));
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites"), 2);
    Ok(())
}

#[test]
fn a_refused_player_recipe_leaves_the_sources_untouched() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let a = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "a.example", Some("jwplayer"), 1, Origin::Llm), t0())?);
    let b = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "b.example", Some("jwplayer"), 1, Origin::Llm), t0())?);
    r.recipe_event(a, "a.example", Event::Approval, t0())?;
    r.recipe_event(b, "b.example", Event::Approval, t0())?;
    let player = recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 1, Origin::Llm);
    r.tombstone_recipe(&player.hash, RetiredReason::Rejected, t0())?;
    assert_eq!(r.promote_to_player(&sha(&json(1)), "jwplayer", &player.hash, &player.recipe_json, 1, t0())?, None);
    assert_eq!(state_of(&c, a).0, "active");
    assert_eq!(state_of(&c, b).0, "active");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE scope_kind = 'player'"), 0);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites"), 2);
    Ok(())
}

#[test]
fn shipped_recipes_sync_idempotently_and_are_never_touched_by_the_rules() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let two = [shipped("one", 1), shipped("two", 2)];
    assert_eq!(r.sync_shipped(&two, t0())?, (2, 0));
    assert_eq!(r.sync_shipped(&two, t0())?, (0, 0));
    let counts = r.recipe_counts()?;
    assert_eq!((counts.shipped, counts.live), (2, 0), "shipped recipes are not counted against the caps");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE state = 'active' AND origin = 'shipped' AND scope_kind = 'generic'"), 2);

    // a site row on a shipped recipe goes with it when the list drops it
    let id: i64 = c.query_row("SELECT id FROM recipes WHERE scope_key = 'two'", [], |r| r.get(0))?;
    r.recipe_event(id, "a.example", Event::Approval, t0())?;
    assert_eq!(r.sync_shipped(&[shipped("one", 1), shipped("three", 3)], t0())?, (1, 1));
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE origin = 'shipped'"), 2);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites"), 0);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones"), 0, "no tombstone for a dropped shipped recipe");

    // a tombstone is refused for a shipped hash, and a tombstone does not stop a sync
    assert!(r.tombstone_recipe(&shipped("one", 1).hash, RetiredReason::Rejected, t0()).is_err());
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones"), 0);

    // gc a long time later: still there, still active
    let rep = r.recipe_gc(t0() + days(2000))?;
    assert_eq!(rep.deleted_recipes + rep.retired_idle, 0);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE origin = 'shipped' AND state = 'active'"), 2);
    Ok(())
}

#[test]
fn gc_retires_idle_recipes_at_ninety_days_not_before() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let id = id_of(r.insert_recipe(&site("a.example", 1, Origin::Llm), t0())?);
    r.recipe_event(id, "a.example", Event::CanaryPass, t0())?;
    let rep = r.recipe_gc(t0() + days(89))?;
    assert_eq!(rep.retired_idle, 0);
    assert_ne!(state_of(&c, id).0, "retired");
    let rep = r.recipe_gc(t0() + days(90))?;
    assert_eq!(rep.retired_idle, 1);
    assert_eq!(state_of(&c, id), ("retired".into(), Some("idle".into())));
    // a use resets the clock
    let id2 = id_of(r.insert_recipe(&site("a.example", 2, Origin::Llm), t0())?);
    r.recipe_event(id2, "a.example", Event::Approval, t0() + days(60))?;
    assert_eq!(r.recipe_gc(t0() + days(140))?.retired_idle, 0);
    assert_eq!(r.recipe_gc(t0() + days(150))?.retired_idle, 1);
    Ok(())
}

#[test]
fn gc_retires_a_recipe_suspended_everywhere_after_thirty_days() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let at = t0() + days(5);
    let rejected = id_of(r.insert_recipe(&site("a.example", 1, Origin::Llm), t0())?);
    r.recipe_event(rejected, "a.example", Event::Rejection, at)?;
    let failing = id_of(r.insert_recipe(&site("b.example", 2, Origin::Llm), t0())?);
    for _ in 0..3 {
        r.recipe_event(failing, "b.example", Event::NoCandidate, at)?;
    }
    let mixed = id_of(r.insert_recipe(&site("c.example", 3, Origin::Llm), t0())?);
    r.recipe_event(mixed, "c.example", Event::Rejection, at)?;
    r.recipe_event(mixed, "d.example", Event::Approval, at)?;

    let rep = r.recipe_gc(at + days(30) - Duration::seconds(1))?;
    assert_eq!((rep.retired_rejected, rep.retired_failing), (0, 0));
    let rep = r.recipe_gc(at + days(30))?;
    assert_eq!((rep.retired_rejected, rep.retired_failing), (1, 1));
    assert_eq!(state_of(&c, rejected), ("retired".into(), Some("rejected".into())));
    assert_eq!(state_of(&c, failing), ("retired".into(), Some("failing".into())));
    assert_ne!(state_of(&c, mixed).0, "retired", "one live site keeps the recipe");
    Ok(())
}

#[test]
fn gc_deletes_retired_recipes_after_ninety_days_and_remembers_why() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let idle = site("a.example", 1, Origin::Llm);
    let rej = site("b.example", 2, Origin::Llm);
    let idle_id = id_of(r.insert_recipe(&idle, t0())?);
    let rej_id = id_of(r.insert_recipe(&rej, t0())?);
    r.recipe_event(rej_id, "b.example", Event::Approval, t0())?;
    r.tombstone_recipe(&rej.hash, RetiredReason::Rejected, t0() + days(10))?;
    let rep = r.recipe_gc(t0() + days(90))?; // idle retires here
    assert_eq!(rep.retired_idle, 1);

    // the rejected one was retired at day 10, so it is due at day 100: not yet at 99
    assert_eq!(r.recipe_gc(t0() + days(99))?.deleted_recipes, 0);
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipes WHERE id = {rej_id}")), 1);
    let rep = r.recipe_gc(t0() + days(100))?;
    assert_eq!(rep.deleted_recipes, 1);
    // the idle one was retired at day 90, so it is still kept at day 179
    assert_eq!(r.recipe_gc(t0() + days(179))?.deleted_recipes, 0);
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipes WHERE id = {idle_id}")), 1);
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipes WHERE id = {rej_id}")), 0);
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipe_sites WHERE recipe_id = {rej_id}")), 0, "its rows go too");
    let rep = r.recipe_gc(t0() + days(180))?;
    assert_eq!(rep.deleted_recipes, 1);
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipes WHERE id = {idle_id}")), 0);

    // remembered: a rejected hash stays refused, an idle one may return
    assert_eq!(r.insert_recipe(&rej, t0() + days(181))?, InsertOutcome::Refused("rejected".into()));
    assert!(matches!(r.insert_recipe(&idle, t0() + days(181))?, InsertOutcome::Inserted(_)));
    Ok(())
}

#[test]
fn gc_expires_tombstones_at_one_hundred_eighty_days() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let new = site("a.example", 1, Origin::Llm);
    r.tombstone_recipe(&new.hash, RetiredReason::Rejected, t0())?;
    let rep = r.recipe_gc(t0() + days(180) - Duration::seconds(1))?;
    assert_eq!(rep.tombstones_expired, 0);
    assert_eq!(r.insert_recipe(&new, t0() + days(179))?, InsertOutcome::Refused("rejected".into()));
    let rep = r.recipe_gc(t0() + days(180))?;
    assert_eq!(rep.tombstones_expired, 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones"), 0);
    assert!(matches!(r.insert_recipe(&new, t0() + days(181))?, InsertOutcome::Inserted(_)));
    Ok(())
}

#[test]
fn gc_holds_the_tombstone_cap_and_keeps_the_newest() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    {
        let tx = c.unchecked_transaction()?;
        for i in 0..(TOMBSTONE_CAP + 52) {
            let at = omni_core::timestamps::format(t0() + Duration::seconds(i as i64));
            tx.execute("INSERT INTO recipe_tombstones (hash, reason, at) VALUES (?, 'idle', ?)", params![format!("h{i:05}"), at])?;
        }
        tx.commit()?;
    }
    let rep = r.recipe_gc(t0() + days(1))?;
    assert_eq!(rep.tombstones_trimmed, 52);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones") as usize, TOMBSTONE_CAP);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones WHERE hash = 'h00051'"), 0, "the oldest went");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones WHERE hash = 'h00052'"), 1, "the newest stayed");
    Ok(())
}

#[test]
fn writing_tombstones_never_goes_past_the_cap() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    for i in 0..(TOMBSTONE_CAP + 20) {
        r.tombstone_recipe(&format!("t{i:05}"), RetiredReason::Idle, t0() + Duration::seconds(i as i64))?;
    }
    assert_eq!(r.recipe_counts()?.tombstones, TOMBSTONE_CAP);
    Ok(())
}

#[test]
fn gc_deletes_stale_site_rows_at_one_hundred_eighty_days() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    r.sync_shipped(&[shipped("one", 1)], t0())?;
    let id: i64 = c.query_row("SELECT id FROM recipes WHERE scope_key = 'one'", [], |r| r.get(0))?;
    r.recipe_event(id, "old.example", Event::Approval, t0())?;
    r.recipe_event(id, "new.example", Event::Approval, t0() + days(10))?;
    assert_eq!(r.recipe_gc(t0() + days(180) - Duration::seconds(1))?.stale_rows, 0);
    assert_eq!(r.recipe_gc(t0() + days(180))?.stale_rows, 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'old.example'"), 0);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'new.example'"), 1);
    Ok(())
}

/// `n` rows on the shipped recipe `id`, oldest first; every 7th suspended.
fn fill_site_rows(c: &Connection, id: i64, n: usize) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    for i in 0..n {
        let at = omni_core::timestamps::format(t0() + Duration::seconds(i as i64));
        let suspended = i % 7 == 0;
        tx.execute(
            "INSERT INTO recipe_sites (recipe_id, domain, scored_at, created_at, state, suspended_reason, suspended_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![id, format!("d{i:05}.example"), at, at, if suspended { "suspended" } else { "candidate" },
                    if suspended { Some("failing") } else { None }, if suspended { Some(at.clone()) } else { None }],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[test]
fn gc_holds_the_site_row_cap_dropping_the_oldest_unsuspended_rows_first() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    r.sync_shipped(&[shipped("one", 1)], t0())?;
    let id: i64 = c.query_row("SELECT id FROM recipes WHERE scope_key = 'one'", [], |r| r.get(0))?;
    fill_site_rows(&c, id, SITE_ROW_CAP + 10)?;
    let rep = r.recipe_gc(t0() + days(1))?;
    assert_eq!(rep.rows_trimmed, 10);
    assert_eq!(r.recipe_counts()?.sites, SITE_ROW_CAP);
    // d00000 is the oldest but suspended; the oldest unsuspended ones went instead
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'd00000.example'"), 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'd00001.example'"), 0);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'd00007.example'"), 1);
    Ok(())
}

#[test]
fn a_new_site_row_at_the_cap_makes_room_instead_of_growing() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    r.sync_shipped(&[shipped("one", 1)], t0())?;
    let shipped_id: i64 = c.query_row("SELECT id FROM recipes WHERE scope_key = 'one'", [], |r| r.get(0))?;
    fill_site_rows(&c, shipped_id, SITE_ROW_CAP)?;
    let id = id_of(r.insert_recipe(&site("a.example", 1, Origin::Llm), t0())?);
    r.recipe_event(id, "a.example", Event::Approval, t0() + days(1))?;
    assert_eq!(r.recipe_counts()?.sites, SITE_ROW_CAP);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'a.example'"), 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'd00001.example'"), 0, "the oldest unsuspended row went");
    // an update of an existing row does not shed anything
    r.recipe_event(id, "a.example", Event::Approval, t0() + days(2))?;
    assert_eq!(r.recipe_counts()?.sites, SITE_ROW_CAP);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'd00002.example'"), 1);
    Ok(())
}

#[test]
fn recipe_states_are_never_left_unknown() {
    // The strings the store writes parse back into the enums the rules use.
    for s in ["candidate", "active", "suspended", "retired"] {
        assert_eq!(RecipeState::parse(s).unwrap().as_str(), s);
    }
    for s in ["idle", "failing", "rejected", "evicted", "superseded", "admin"] {
        assert_eq!(RetiredReason::parse(s).unwrap().as_str(), s);
    }
}

// ---------------------------------------------------------------------------
// Simulation: 10 000 random operations; the caps and the tombstone rule hold
// after every hundred of them.
// ---------------------------------------------------------------------------

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn simulation_of_ten_thousand_events_never_breaks_a_cap_or_resurrects_a_rejected_recipe() -> Result<()> {
    let started = std::time::Instant::now();
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let mut rng = Lcg(0x5EED_1234_ABCD);
    let mut now = t0();
    const FAMILIES: [&str; 3] = ["jwplayer", "videojs", "brightcove"];
    const SITES: u64 = 50;

    r.sync_shipped(&(0..5).map(|i| shipped(&format!("gen{i}"), i)).collect::<Vec<_>>(), now)?;

    let mut known: Vec<(i64, String, Option<String>)> = Vec::new(); // id, hash, own domain of a site recipe
    // hash -> when the store was first seen to forbid it
    let mut forbidden: std::collections::HashMap<String, DateTime<Utc>> = std::collections::HashMap::new();
    let (mut inserted, mut revived, mut refused, mut over_cap, mut merged, mut promoted, mut deleted) = (0, 0, 0, 0, 0, 0, 0);
    let mut max_tombstones = 0usize;
    // (steps hash, domain) -> latest time MCR rejected those steps there
    let mut rejections: std::collections::HashMap<(String, String), DateTime<Utc>> = std::collections::HashMap::new();
    let mut max_bars = 0usize;
    let mut max_rows = 0usize;
    let mut max_bytes = 0usize;

    for step in 1..=10_000u32 {
        now += Duration::minutes(rng.below(360) as i64);
        match rng.below(100) {
            0..=29 => {
                let hot = rng.below(2) == 0;
                let n = if hot { rng.below(12) } else { 40 + rng.below(700) } as usize;
                let family = FAMILIES[n % 3];
                let origin = if rng.below(100) < 15 { Origin::Admin } else { Origin::Llm };
                let new = if rng.below(100) < 70 {
                    let d = format!("site{}.example", rng.below(SITES));
                    recipe(ScopeKind::Site, &d, if hot || rng.below(2) == 0 { Some(family) } else { None }, n, origin)
                } else {
                    recipe(ScopeKind::Player, family, Some(family), n, origin)
                };
                let out = r.insert_recipe(&new, now)?;
                match &out {
                    InsertOutcome::Inserted(id) => {
                        inserted += 1;
                        known.push((*id, new.hash.clone(), (new.scope_kind == ScopeKind::Site).then(|| new.scope_key.clone())));
                    }
                    InsertOutcome::Revived(id) => {
                        revived += 1;
                        known.push((*id, new.hash.clone(), (new.scope_kind == ScopeKind::Site).then(|| new.scope_key.clone())));
                    }
                    InsertOutcome::Merged(id) => {
                        merged += 1;
                        known.push((*id, new.hash.clone(), (new.scope_kind == ScopeKind::Site).then(|| new.scope_key.clone())));
                    }
                    InsertOutcome::Refused(_) => refused += 1,
                    InsertOutcome::OverCap => over_cap += 1,
                }
                if matches!(out, InsertOutcome::Inserted(_) | InsertOutcome::Revived(_)) {
                    if let Some(since) = forbidden.get(&new.hash) {
                        assert!(
                            now - *since >= days(175),
                            "step {step}: {} came back {} days after it was forbidden",
                            new.hash,
                            (now - *since).num_days()
                        );
                    }
                }
            }
            30..=79 => {
                if known.is_empty() {
                    continue;
                }
                let (id, _, own) = known[rng.below(known.len() as u64) as usize].clone();
                // half the events are on the recipe's own site, where proof can build up
                let domain = match own {
                    Some(d) if rng.below(2) == 0 => d,
                    _ => format!("site{}.example", rng.below(SITES)),
                };
                let event = match rng.below(100) {
                    0..=24 => Event::Approval,
                    25..=29 => Event::Rejection,
                    30..=54 => Event::CanaryPass,
                    55..=69 => Event::CanaryFail,
                    70..=84 => Event::NoCandidate,
                    _ => Event::AutoDelivered,
                };
                match r.recipe_event(id, &domain, event, now) {
                    Err(_) => known.retain(|k| k.0 != id), // deleted by gc
                    Ok(_) if event == Event::Rejection => {
                        let steps: String =
                            c.query_row("SELECT steps_hash FROM recipes WHERE id = ?", params![id], |row| row.get(0))?;
                        rejections.insert((steps, domain.clone()), now);
                    }
                    Ok(_) => {}
                }
            }
            80..=84 => {
                let n = rng.below(12) as usize;
                let family = FAMILIES[n % 3];
                let player = recipe(ScopeKind::Player, family, Some(family), n, Origin::Llm);
                if let Some(pid) = r.promote_to_player(&player.steps_hash, family, &player.hash, &player.recipe_json, 1, now)? {
                    promoted += 1;
                    known.push((pid, player.hash.clone(), None));
                    if let Some(since) = forbidden.get(&player.hash) {
                        assert!(now - *since >= days(175), "step {step}: a forbidden player recipe was promoted");
                    }
                }
            }
            85..=87 => {
                if known.is_empty() {
                    continue;
                }
                let (_, hash, _) = known[rng.below(known.len() as u64) as usize].clone();
                r.tombstone_recipe(&hash, RetiredReason::Rejected, now)?;
                forbidden.insert(hash, now);
            }
            _ => {
                let domain = format!("site{}.example", rng.below(SITES));
                let family = FAMILIES[rng.below(3) as usize];
                let got = r.recipes_for_site(&domain, Some(family), now)?;
                for g in &got {
                    let (state, _) = state_of(&c, g.id);
                    assert_ne!(state, "retired", "step {step}: a retired recipe was offered");
                    assert_ne!(g.site_state, Some(SiteState::Suspended), "step {step}: a suspended recipe was offered");
                    if let Some(at) = rejections.get(&(g.steps_hash.clone(), domain.clone())) {
                        assert!(now - *at >= days(180), "step {step}: steps rejected on {domain} {} days ago were offered", (now - *at).num_days());
                    }
                }
            }
        }
        if step % 500 == 0 {
            deleted += r.recipe_gc(now)?.deleted_recipes;
        }
        if step % 100 == 0 {
            // what the store itself now forbids
            let mut stmt = c.prepare(
                "SELECT hash, COALESCE(retired_at, created_at) FROM recipes WHERE retired_reason = 'rejected'
                 UNION ALL SELECT hash, at FROM recipe_tombstones WHERE reason IN ('rejected', 'admin')",
            )?;
            let found = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (hash, at) in found {
                let at = omni_core::timestamps::parse(&at).expect("stored time parses");
                let e = forbidden.entry(hash).or_insert(at);
                *e = (*e).min(at);
            }

            let over_scope = count(
                &c,
                "SELECT COUNT(*) FROM (SELECT 1 FROM recipes WHERE state <> 'retired' AND origin <> 'shipped'
                   AND scope_kind IN ('site', 'player') GROUP BY scope_kind, scope_key HAVING COUNT(*) > 4)",
            );
            assert_eq!(over_scope, 0, "step {step}: a scope holds more than 4 live recipes");
            let counts = r.recipe_counts()?;
            let rows = count(&c, "SELECT COUNT(*) FROM recipes WHERE origin <> 'shipped'") as usize;
            assert!(counts.live <= NONSHIPPED_LIVE_CAP, "step {step}: {} live", counts.live);
            assert!(rows <= NONSHIPPED_ROW_CAP, "step {step}: {rows} rows");
            assert!(counts.sites <= SITE_ROW_CAP, "step {step}: {} site rows", counts.sites);
            assert!(counts.tombstones <= TOMBSTONE_CAP, "step {step}: {} tombstones", counts.tombstones);
            assert!(counts.json_bytes < 2 << 20, "step {step}: {} bytes of recipes", counts.json_bytes);
            assert_eq!(count(&c, "SELECT COUNT(*) FROM (SELECT hash FROM recipes GROUP BY hash HAVING COUNT(*) > 1)"), 0);
            assert_eq!(counts.shipped, 5, "step {step}: shipped recipes must be untouched");
            assert_eq!(
                count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE recipe_id NOT IN (SELECT id FROM recipes)"),
                0,
                "step {step}: orphaned site rows"
            );
            max_tombstones = max_tombstones.max(counts.tombstones);
            assert!(counts.bars <= BAR_CAP, "step {step}: {} bars", counts.bars);
            max_bars = max_bars.max(counts.bars);
            max_rows = max_rows.max(rows);
            max_bytes = max_bytes.max(counts.json_bytes);
        }
    }

    let elapsed = started.elapsed();
    eprintln!(
        "simulation: {elapsed:?}; inserted {inserted}, merged {merged}, revived {revived}, refused {refused}, \
         over_cap {over_cap}, promoted {promoted}, deleted by gc {deleted}; max rows {max_rows}, \
         max tombstones {max_tombstones}, max bars {max_bars}, rejections {}, max json bytes {max_bytes}; simulated {} days",
        rejections.len(),
        (now - t0()).num_days()
    );
    // The run must have exercised what it claims to check.
    assert!(inserted > 500, "only {inserted} inserts");
    assert!(refused > 0, "no insert was ever refused, the tombstone rule was not exercised");
    assert!(promoted > 0, "no promotion happened");
    assert!(deleted > 0, "gc never deleted a recipe");
    assert!(max_rows >= NONSHIPPED_ROW_CAP - 5, "the row cap was never approached ({max_rows})");
    Ok(())
}

// ---------------------------------------------------------------------------
// Review fixes: a rejection survives sharing, rejected rows and tombstones
// outlast the caps, a revived recipe gets its idle clock back, and an admin
// re-add keeps the recipe.
// ---------------------------------------------------------------------------

/// Site X rejects steps H; later A and B prove H and they are promoted to a
/// player recipe. The recipe offered on A and B must never be offered on X.
fn rejected_then_promoted(dir: &TempDir, gc_the_rejected_one: bool) -> Result<(Repository, i64)> {
    let r = repo(dir);
    let fam = Some("jwplayer");
    let sx = recipe(ScopeKind::Site, "x.example", fam, 1, Origin::Llm);
    let x = id_of(r.insert_recipe(&sx, t0())?);
    let a = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "a.example", fam, 1, Origin::Llm), t0())?);
    let b = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "b.example", fam, 1, Origin::Llm), t0())?);
    r.recipe_event(x, "x.example", Event::Rejection, t0() + days(1))?;
    r.recipe_event(a, "a.example", Event::Approval, t0() + days(1))?;
    r.recipe_event(b, "b.example", Event::Approval, t0() + days(1))?;
    let rep = r.recipe_gc(t0() + days(31))?;
    assert_eq!(rep.retired_rejected, 1);
    let mut when = t0() + days(32);
    if gc_the_rejected_one {
        // keep A and B in use, then let the rejected recipe be deleted
        r.recipe_event(a, "a.example", Event::Approval, t0() + days(100))?;
        r.recipe_event(b, "b.example", Event::Approval, t0() + days(100))?;
        assert_eq!(r.recipe_gc(t0() + days(122))?.deleted_recipes, 1);
        assert_eq!(count(&raw(dir), &format!("SELECT COUNT(*) FROM recipes WHERE id = {x}")), 0);
        when = t0() + days(123);
    }
    let player = recipe(ScopeKind::Player, "jwplayer", fam, 1, Origin::Llm);
    let pid = r
        .promote_to_player(&sha(&json(1)), "jwplayer", &player.hash, &player.recipe_json, 1, when)?
        .expect("promoted");
    Ok((r, pid))
}

#[test]
fn a_rejection_on_a_site_survives_the_steps_becoming_a_shared_player_recipe() -> Result<()> {
    let dir = TempDir::new()?;
    let (r, pid) = rejected_then_promoted(&dir, false)?;
    let now = t0() + days(40);
    let ids = |d: &str| -> Vec<i64> { r.recipes_for_site(d, Some("jwplayer"), now).unwrap().iter().map(|x| x.id).collect() };
    assert!(!ids("x.example").contains(&pid), "the steps MCR rejected on X were offered there");
    assert_eq!(ids("a.example"), vec![pid]);
    assert_eq!(ids("b.example"), vec![pid]);
    // the bar does that job: promotion adds no row for X
    let c = raw(&dir);
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipe_sites WHERE recipe_id = {pid} AND domain = 'x.example'")), 0);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_bars WHERE domain = 'x.example'"), 1);
    // re-adding the rejected recipe is refused, not merged into the player recipe
    let sx = recipe(ScopeKind::Site, "x.example", Some("jwplayer"), 1, Origin::Llm);
    assert_eq!(r.insert_recipe(&sx, now)?, InsertOutcome::Refused("rejected".into()));
    Ok(())
}

#[test]
fn a_rejection_survives_even_when_only_its_tombstone_is_left() -> Result<()> {
    let dir = TempDir::new()?;
    let (r, pid) = rejected_then_promoted(&dir, true)?;
    let now = t0() + days(130);
    let got: Vec<i64> = r.recipes_for_site("x.example", Some("jwplayer"), now)?.iter().map(|x| x.id).collect();
    assert!(!got.contains(&pid), "P offered on X after the rejected recipe was deleted");
    let got: Vec<i64> = r.recipes_for_site("a.example", Some("jwplayer"), now)?.iter().map(|x| x.id).collect();
    assert_eq!(got, vec![pid]);
    // the tombstone knows the steps and the site
    let c = raw(&dir);
    let (steps, kind, key): (Option<String>, Option<String>, Option<String>) = c.query_row(
        "SELECT steps_hash, scope_kind, scope_key FROM recipe_tombstones WHERE reason = 'rejected'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!((steps, kind.as_deref(), key.as_deref()), (Some(sha(&json(1))), Some("site"), Some("x.example")));
    Ok(())
}

#[test]
fn a_shipped_recipe_with_the_steps_rejected_on_a_site_is_not_offered_there() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    r.sync_shipped(&[shipped("g", 1)], t0())?;
    // a site recipe with the very same steps as the shipped one (json 1001)
    let id = id_of(r.insert_recipe(&site("d.example", 1001, Origin::Llm), t0())?);
    let names = |d: &str, n| -> Vec<String> {
        r.recipes_for_site(d, None, n).unwrap().iter().filter(|x| x.scope_key == "g").map(|x| x.scope_key.clone()).collect()
    };
    assert_eq!(names("d.example", t0()), vec!["g".to_string()]);
    r.recipe_event(id, "d.example", Event::Rejection, t0())?;
    assert_eq!(names("d.example", t0() + days(1)), Vec::<String>::new());
    assert_eq!(names("other.example", t0() + days(1)), vec!["g".to_string()]);
    Ok(())
}

#[test]
fn gc_keeps_rejected_rows_however_old_but_drops_old_failing_ones() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    r.sync_shipped(&[shipped("one", 1)], t0())?;
    let id: i64 = c.query_row("SELECT id FROM recipes WHERE scope_key = 'one'", [], |r| r.get(0))?;
    let at = omni_core::timestamps::format(t0());
    for (dom, reason) in [("rej.example", "rejected"), ("fail.example", "failing")] {
        c.execute(
            "INSERT INTO recipe_sites (recipe_id, domain, scored_at, created_at, state, suspended_reason, suspended_at)
             VALUES (?, ?, ?, ?, 'suspended', ?, ?)",
            params![id, dom, at, at, reason, at],
        )?;
    }
    let rep = r.recipe_gc(t0() + days(200))?;
    assert_eq!(rep.stale_rows, 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'rej.example'"), 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites WHERE domain = 'fail.example'"), 0);
    Ok(())
}

#[test]
fn the_tombstone_cap_drops_forgettable_tombstones_before_a_rejected_one() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let rejected = site("a.example", 1, Origin::Llm);
    r.tombstone_recipe(&rejected.hash, RetiredReason::Rejected, t0())?;
    for i in 0..TOMBSTONE_CAP {
        r.tombstone_recipe(&format!("e{i:05}"), RetiredReason::Evicted, t0() + Duration::seconds(i as i64 + 1))?;
    }
    assert_eq!(r.recipe_counts()?.tombstones, TOMBSTONE_CAP);
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipe_tombstones WHERE hash = '{}'", rejected.hash)), 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_tombstones WHERE hash = 'e00000'"), 0, "the oldest forgettable one went");
    assert_eq!(r.insert_recipe(&rejected, t0() + days(1))?, InsertOutcome::Refused("rejected".into()));

    // the same in gc, with rows written behind the store's back
    let dir2 = TempDir::new()?;
    let r2 = repo(&dir2);
    let c2 = raw(&dir2);
    let tx = c2.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO recipe_tombstones (hash, reason, at) VALUES ('keep', 'rejected', ?)",
        params![omni_core::timestamps::format(t0())],
    )?;
    for i in 0..(TOMBSTONE_CAP + 5) {
        tx.execute(
            "INSERT INTO recipe_tombstones (hash, reason, at) VALUES (?, 'idle', ?)",
            params![format!("i{i:05}"), omni_core::timestamps::format(t0() + Duration::seconds(i as i64 + 1))],
        )?;
    }
    tx.commit()?;
    let rep = r2.recipe_gc(t0() + days(1))?;
    assert_eq!(rep.tombstones_trimmed, 6);
    assert_eq!(count(&c2, "SELECT COUNT(*) FROM recipe_tombstones WHERE hash = 'keep'"), 1);
    assert_eq!(count(&c2, "SELECT COUNT(*) FROM recipe_tombstones") as usize, TOMBSTONE_CAP);
    Ok(())
}

#[test]
fn a_revived_recipe_starts_its_idle_clock_again() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let new = site("a.example", 1, Origin::Llm);
    let id = id_of(r.insert_recipe(&new, t0())?);
    assert_eq!(r.recipe_gc(t0() + days(100))?.retired_idle, 1);
    assert_eq!(r.insert_recipe(&new, t0() + days(101))?, InsertOutcome::Revived(id));
    assert_eq!(r.recipe_gc(t0() + days(102))?.retired_idle, 0, "retired again the next night");
    assert_ne!(state_of(&c, id).0, "retired");
    assert_eq!(r.recipe_gc(t0() + days(101) + days(89))?.retired_idle, 0);
    assert_eq!(r.recipe_gc(t0() + days(101) + days(90))?.retired_idle, 1);
    assert_eq!(state_of(&c, id), ("retired".into(), Some("idle".into())));
    Ok(())
}

#[test]
fn an_admin_re_add_makes_a_retired_llm_recipe_an_admin_recipe() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let llm = site("a.example", 1, Origin::Llm);
    let rid = id_of(r.insert_recipe(&llm, t0())?);
    r.insert_recipe(&site("a.example", 2, Origin::Admin), t0())?;
    r.insert_recipe(&site("a.example", 3, Origin::Admin), t0())?;
    r.tombstone_recipe(&llm.hash, RetiredReason::Idle, t0())?;
    r.insert_recipe(&site("a.example", 4, Origin::Admin), t0())?;
    r.insert_recipe(&site("a.example", 5, Origin::Admin), t0())?;
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes WHERE state <> 'retired' AND origin = 'admin'"), 4);

    let readd = site("a.example", 1, Origin::Admin);
    assert_eq!(r.insert_recipe(&readd, t0() + days(2))?, InsertOutcome::Revived(rid));
    let origin: String = c.query_row("SELECT origin FROM recipes WHERE id = ?", params![rid], |r| r.get(0))?;
    assert_eq!(origin, "admin");
    // the scope is full of admin recipes: an llm insert cannot push R out
    assert_eq!(r.insert_recipe(&site("a.example", 99, Origin::Llm), t0() + days(3))?, InsertOutcome::OverCap);
    assert_ne!(state_of(&c, rid).0, "retired", "R was evicted");
    Ok(())
}

// ---------------------------------------------------------------------------
// Bars: "these steps produced a video MCR rejected on this domain", whichever
// recipe carried them. Every rejection here is made with `recipe_event`.
// ---------------------------------------------------------------------------

fn offered(r: &Repository, domain: &str, family: Option<&str>, now: DateTime<Utc>) -> Vec<i64> {
    r.recipes_for_site(domain, family, now).unwrap().iter().map(|x| x.id).collect()
}

fn generic_id(r: &Repository, name: &str) -> i64 {
    r.recipes_for_site("probe.example", None, t0()).unwrap().iter().find(|x| x.scope_key == name).unwrap().id
}

#[test]
fn a_rejected_shipped_recipe_bars_its_steps_for_site_recipes_on_that_domain() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    r.sync_shipped(&[shipped("g", 1)], t0())?;
    let g = generic_id(&r, "g");
    // a site recipe for D with the shipped recipe's steps already exists
    let early = id_of(r.insert_recipe(&site("d.example", 1001, Origin::Llm), t0())?);
    assert_eq!(offered(&r, "d.example", None, t0()), vec![early, g]);

    r.recipe_event(g, "d.example", Event::Rejection, t0() + days(1))?;
    let now = t0() + days(2);
    assert_eq!(offered(&r, "d.example", None, now), Vec::<i64>::new(), "neither carries the rejected steps");
    assert_eq!(offered(&r, "other.example", None, now), vec![g], "other domains still get the shipped recipe");
    // the LLM cannot propose the same steps for D again
    let again = site("d.example", 1001, Origin::Llm);
    let rows = r.recipe_counts()?.live;
    assert_eq!(r.insert_recipe(&again, now)?, InsertOutcome::Refused("rejected".into()));
    assert_eq!(r.recipe_counts()?.live, rows);
    // but another domain may have them
    let e = id_of(r.insert_recipe(&site("e.example", 1001, Origin::Llm), now)?);
    assert_eq!(offered(&r, "e.example", None, now), vec![e, g]);
    Ok(())
}

#[test]
fn a_rejected_player_recipe_bars_its_steps_on_that_domain_only() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let p = id_of(r.insert_recipe(&recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 1, Origin::Llm), t0())?);
    r.recipe_event(p, "a.example", Event::Approval, t0())?;
    r.recipe_event(p, "d.example", Event::Rejection, t0() + days(1))?;
    let now = t0() + days(2);
    assert_eq!(
        r.insert_recipe(&recipe(ScopeKind::Site, "d.example", None, 1, Origin::Llm), now)?,
        InsertOutcome::Refused("rejected".into())
    );
    assert_eq!(
        r.insert_recipe(&recipe(ScopeKind::Site, "d.example", Some("videojs"), 1, Origin::Llm), now)?,
        InsertOutcome::Refused("rejected".into())
    );
    assert!(!offered(&r, "d.example", Some("jwplayer"), now).contains(&p));
    assert_eq!(offered(&r, "a.example", Some("jwplayer"), now), vec![p]);
    // a player and a site recipe on another domain are untouched
    assert!(matches!(r.insert_recipe(&recipe(ScopeKind::Site, "a.example", None, 1, Origin::Llm), now)?, InsertOutcome::Inserted(_)));
    Ok(())
}

#[test]
fn a_site_recipe_rejected_on_another_domain_bars_its_steps_in_every_tier_there() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    r.sync_shipped(&[shipped("g", 1)], t0())?;
    let g = generic_id(&r, "g");
    let from_a = id_of(r.insert_recipe(&site("a.example", 1001, Origin::Llm), t0())?); // steps of g
    let player = id_of(r.insert_recipe(&recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 1001, Origin::Llm), t0())?);
    let unrelated = id_of(r.insert_recipe(&recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 7, Origin::Llm), t0())?);
    let before = offered(&r, "d.example", Some("jwplayer"), t0());
    assert!(before.contains(&player) && before.contains(&unrelated) && before.contains(&g), "{before:?}");

    // the recipe of A is rejected on D (it was tried there)
    r.recipe_event(from_a, "d.example", Event::Rejection, t0() + days(1))?;
    let now = t0() + days(2);
    let on_d = offered(&r, "d.example", Some("jwplayer"), now);
    assert_eq!(on_d, vec![unrelated], "player and generic with the rejected steps are gone on D: {on_d:?}");
    let on_e = offered(&r, "e.example", Some("jwplayer"), now);
    assert!(on_e.contains(&player) && on_e.contains(&g) && on_e.contains(&unrelated));
    Ok(())
}

#[test]
fn a_bar_outlives_the_deleted_recipe_and_expires_after_one_hundred_eighty_days() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let player = recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 1, Origin::Llm);
    let p = id_of(r.insert_recipe(&player, t0())?);
    r.recipe_event(p, "d.example", Event::Rejection, t0())?;
    // evicted, then deleted by gc: the recipe and its rejected row are gone
    r.tombstone_recipe(&player.hash, RetiredReason::Evicted, t0() + days(1))?;
    assert_eq!(r.recipe_gc(t0() + days(92))?.deleted_recipes, 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipes"), 0);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_sites"), 0);

    // the same steps come back as a new player recipe
    let p2 = id_of(r.insert_recipe(&player, t0() + days(93))?);
    assert_eq!(offered(&r, "d.example", Some("jwplayer"), t0() + days(94)), Vec::<i64>::new());
    assert_eq!(offered(&r, "a.example", Some("jwplayer"), t0() + days(94)), vec![p2]);

    // 179 days: still barred; 180: expired
    let rep = r.recipe_gc(t0() + days(180) - Duration::seconds(1))?;
    assert_eq!(rep.bars_expired, 0);
    assert_eq!(offered(&r, "d.example", Some("jwplayer"), t0() + days(179)), Vec::<i64>::new());
    let rep = r.recipe_gc(t0() + days(180))?;
    assert_eq!(rep.bars_expired, 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM recipe_bars"), 0);
    assert_eq!(offered(&r, "d.example", Some("jwplayer"), t0() + days(181)), vec![p2]);
    Ok(())
}

#[test]
fn an_admin_tombstone_of_a_site_recipe_bars_its_steps_on_that_site() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    r.sync_shipped(&[shipped("g", 1)], t0())?;
    let g = generic_id(&r, "g");
    let s = site("d.example", 1001, Origin::Admin);
    r.insert_recipe(&s, t0())?;
    r.tombstone_recipe(&s.hash, RetiredReason::Admin, t0() + days(1))?;
    assert_eq!(offered(&r, "d.example", None, t0() + days(2)), Vec::<i64>::new(), "the shipped one has the same steps");
    assert_eq!(offered(&r, "other.example", None, t0() + days(2)), vec![g]);
    // an idle tombstone bars nothing
    let t = site("e.example", 1001, Origin::Llm);
    r.insert_recipe(&t, t0())?;
    r.tombstone_recipe(&t.hash, RetiredReason::Idle, t0() + days(1))?;
    assert_eq!(offered(&r, "e.example", None, t0() + days(2)), vec![g]);
    Ok(())
}

#[test]
fn the_bar_cap_holds_at_write_time_and_in_gc() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    // 52 recipes with 40 domains each, so no recipe holds thousands of rows
    let ids: Vec<i64> = (0..52)
        .map(|k| id_of(r.insert_recipe(&site(&format!("s{k}.example"), k + 1, Origin::Llm), t0()).unwrap()))
        .collect();
    let steps = sha(&json(1));
    for i in 0..(BAR_CAP + 10) {
        r.recipe_event(ids[i / 40], &format!("d{i:05}.example"), Event::Rejection, t0() + Duration::seconds(i as i64))?;
    }
    assert_eq!(r.recipe_counts()?.bars, BAR_CAP);
    let barred = |d: &str| count(&c, &format!("SELECT COUNT(*) FROM recipe_bars WHERE steps_hash = '{steps}' AND domain = '{d}'"));
    assert_eq!(barred("d00000.example"), 0, "the oldest bar went");
    assert_eq!(barred("d00010.example"), 1);

    let dir2 = TempDir::new()?;
    let r2 = repo(&dir2);
    let c2 = raw(&dir2);
    let tx = c2.unchecked_transaction()?;
    for i in 0..(BAR_CAP + 5) {
        tx.execute(
            "INSERT INTO recipe_bars (steps_hash, domain, at) VALUES ('h', ?, ?)",
            params![format!("d{i:05}.example"), omni_core::timestamps::format(t0() + Duration::seconds(i as i64))],
        )?;
    }
    tx.commit()?;
    let rep = r2.recipe_gc(t0() + days(1))?;
    assert_eq!(rep.bars_trimmed, 5);
    assert_eq!(r2.recipe_counts()?.bars, BAR_CAP);
    assert_eq!(count(&c2, "SELECT COUNT(*) FROM recipe_bars WHERE domain = 'd00004.example'"), 0);
    assert_eq!(count(&c2, "SELECT COUNT(*) FROM recipe_bars WHERE domain = 'd00005.example'"), 1);
    Ok(())
}

#[test]
fn promotion_writes_no_extra_rows_and_does_not_make_a_player_recipe_look_rejected() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let fam = Some("jwplayer");
    let x = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "x.example", fam, 1, Origin::Llm), t0())?);
    let a = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "a.example", fam, 1, Origin::Llm), t0())?);
    let b = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "b.example", fam, 1, Origin::Llm), t0())?);
    r.recipe_event(x, "x.example", Event::Rejection, t0())?;
    r.recipe_event(a, "a.example", Event::Approval, t0())?;
    r.recipe_event(b, "b.example", Event::Approval, t0())?;
    let before = r.recipe_counts()?.sites;
    let player = recipe(ScopeKind::Player, "jwplayer", fam, 1, Origin::Llm);
    let pid = r.promote_to_player(&sha(&json(1)), "jwplayer", &player.hash, &player.recipe_json, 1, t0())?.expect("promoted");
    // nothing new was written
    assert_eq!(r.recipe_counts()?.sites, before);
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipe_sites WHERE recipe_id = {pid}")), 2, "a and b moved; the rejected row of x stayed behind");

    // a player recipe whose only suspended row is a failing one, while the same steps
    // were rejected elsewhere, is retired as failing, not as rejected
    let dir2 = TempDir::new()?;
    let r2 = repo(&dir2);
    let c2 = raw(&dir2);
    let x2 = id_of(r2.insert_recipe(&recipe(ScopeKind::Site, "x.example", fam, 1, Origin::Llm), t0())?);
    r2.recipe_event(x2, "x.example", Event::Rejection, t0())?;
    let lone = id_of(r2.insert_recipe(&player, t0())?);
    for _ in 0..3 {
        r2.recipe_event(lone, "z.example", Event::CanaryFail, t0() + days(1))?;
    }
    r2.recipe_gc(t0() + days(31) + Duration::seconds(1))?;
    assert_eq!(state_of(&c2, lone), ("retired".into(), Some("failing".into())));
    assert_eq!(state_of(&c2, x2), ("retired".into(), Some("rejected".into())));
    Ok(())
}

#[test]
fn a_rejected_source_row_is_not_moved_so_an_outage_elsewhere_retires_the_player_as_failing() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let c = raw(&dir);
    let fam = Some("jwplayer");
    let x = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "x.example", fam, 1, Origin::Llm), t0())?);
    let a = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "a.example", fam, 1, Origin::Llm), t0())?);
    let b = id_of(r.insert_recipe(&recipe(ScopeKind::Site, "b.example", fam, 1, Origin::Llm), t0())?);
    r.recipe_event(x, "x.example", Event::Rejection, t0())?;
    r.recipe_event(a, "a.example", Event::Approval, t0())?;
    r.recipe_event(b, "b.example", Event::Approval, t0())?;
    let player = recipe(ScopeKind::Player, "jwplayer", fam, 1, Origin::Llm);
    let pid = r.promote_to_player(&sha(&json(1)), "jwplayer", &player.hash, &player.recipe_json, 1, t0())?.expect("promoted");
    assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM recipe_sites WHERE recipe_id = {pid} AND domain = 'x.example'")), 0);
    assert!(!offered(&r, "x.example", Some("jwplayer"), t0() + days(1)).contains(&pid), "the bar keeps P off x");

    // an outage on a and b: three failures each
    let out = t0() + days(2);
    for _ in 0..3 {
        r.recipe_event(pid, "a.example", Event::CanaryFail, out)?;
        r.recipe_event(pid, "b.example", Event::CanaryFail, out)?;
    }
    let rep = r.recipe_gc(out + days(30))?;
    assert_eq!((rep.retired_failing, rep.retired_rejected), (1, 0), "P retires as failing, never as rejected");
    assert_eq!(state_of(&c, pid), ("retired".into(), Some("failing".into())));
    Ok(())
}

#[test]
fn a_rejected_tombstone_of_a_site_recipe_bars_its_steps_without_any_event() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    r.sync_shipped(&[shipped("g", 1)], t0())?;
    let g = generic_id(&r, "g");
    let p = id_of(r.insert_recipe(&recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 1001, Origin::Llm), t0())?);
    let s = site("d.example", 1001, Origin::Llm);
    r.insert_recipe(&s, t0())?;
    r.tombstone_recipe(&s.hash, RetiredReason::Rejected, t0() + days(1))?;
    let now = t0() + days(2);
    let on_d = offered(&r, "d.example", Some("jwplayer"), now);
    assert!(!on_d.contains(&g) && !on_d.contains(&p), "{on_d:?}");
    let on_e = offered(&r, "e.example", Some("jwplayer"), now);
    assert!(on_e.contains(&g) && on_e.contains(&p));
    let other_family = recipe(ScopeKind::Site, "d.example", Some("videojs"), 1001, Origin::Llm);
    assert_eq!(r.insert_recipe(&other_family, now)?, InsertOutcome::Refused("rejected".into()));
    Ok(())
}

// ---------------------------------------------------------------------------
// P10.14: the recipes the overnight self-heal tries on a site's canary pages.
// ---------------------------------------------------------------------------

fn canary_offered(r: &Repository, domain: &str, family: Option<&str>, now: DateTime<Utc>) -> Vec<i64> {
    r.recipes_for_canary(domain, family, now).unwrap().iter().map(|x| x.id).collect()
}

#[test]
fn recipes_for_canary_adds_only_failing_suspensions() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let now = t0() + days(1);
    let s_fine = id_of(r.insert_recipe(&site("a.example", 1, Origin::Llm), t0())?);
    let s_failing = id_of(r.insert_recipe(&site("a.example", 2, Origin::Llm), t0())?);
    let s_rejected = id_of(r.insert_recipe(&site("a.example", 3, Origin::Llm), t0())?);
    let s_barred = id_of(r.insert_recipe(&site("a.example", 4, Origin::Llm), t0())?);
    let p_failing = id_of(r.insert_recipe(&recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 5, Origin::Llm), t0())?);
    r.sync_shipped(&[shipped("g_failing", 1)], t0())?;
    let g_failing = r.recipes_for_site("zzz.example", None, now)?.iter().find(|x| x.scope_key == "g_failing").unwrap().id;

    for id in [s_failing, p_failing, g_failing] {
        for _ in 0..3 {
            r.recipe_event(id, "a.example", Event::NoCandidate, t0())?;
        }
    }
    r.recipe_event(s_rejected, "a.example", Event::Rejection, t0())?;
    // MCR rejected the steps of s_barred on this domain through another recipe carrying them.
    r.recipe_event(s_barred, "a.example", Event::Approval, t0())?;
    let carrier = id_of(r.insert_recipe(&site("a.example", 4, Origin::Admin), t0())?);
    assert_eq!(carrier, s_barred, "same steps merge into one recipe");
    r.recipe_event(s_barred, "a.example", Event::Rejection, t0())?;
    assert!(r.recipe_steps_barred(&sha(&json(4)), "a.example")?, "the bar is set");

    // A rejection whose bar has expired: the recipe stays suspended as rejected, and is still never tried.
    let s_expired = id_of(r.insert_recipe(&recipe(ScopeKind::Player, "jwplayer", Some("jwplayer"), 6, Origin::Llm), t0())?);
    r.recipe_event(s_expired, "a.example", Event::Rejection, t0())?;
    assert!(r.recipe_steps_barred(&sha(&json(6)), "a.example")?, "a rejection bars its steps");
    raw(&dir).execute("DELETE FROM recipe_bars WHERE steps_hash = ?", params![sha(&json(6))])?;
    assert!(!r.recipe_steps_barred(&sha(&json(6)), "a.example")?);

    // The job path offers none of the five; the self-heal adds the ones suspended as failing.
    let normal = offered(&r, "a.example", Some("jwplayer"), now);
    assert!(![s_failing, s_rejected, s_barred, p_failing, g_failing].iter().any(|i| normal.contains(i)), "{normal:?}");
    assert!(normal.contains(&s_fine));
    let canary = canary_offered(&r, "a.example", Some("jwplayer"), now);
    assert!(canary.contains(&s_fine));
    assert!(canary.contains(&s_failing), "a recipe suspended as failing is tried again");
    assert!(canary.contains(&p_failing), "a player recipe suspended as failing is tried again");
    assert!(canary.contains(&g_failing), "a shipped recipe suspended as failing is tried again too");
    assert!(!canary.contains(&s_rejected), "a recipe MCR rejected is never tried");
    assert!(!canary.contains(&s_barred), "barred steps are never tried");
    assert!(!canary.contains(&s_expired), "a rejected suspension is never lifted into the canary list, bar or no bar");
    Ok(())
}

#[test]
fn two_canary_passes_lift_a_failing_suspension_and_the_job_path_offers_it_again() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let id = id_of(r.insert_recipe(&site("a.example", 1, Origin::Llm), t0())?);
    for _ in 0..3 {
        r.recipe_event(id, "a.example", Event::NoCandidate, t0())?;
    }
    assert!(!offered(&r, "a.example", None, t0() + days(1)).contains(&id), "suspended");
    r.recipe_event(id, "a.example", Event::CanaryPass, t0() + days(1))?;
    assert!(!offered(&r, "a.example", None, t0() + days(1)).contains(&id), "one pass is not enough");
    r.recipe_event(id, "a.example", Event::CanaryPass, t0() + days(1))?;
    let now = t0() + days(1);
    assert!(offered(&r, "a.example", None, now).contains(&id), "two passes lift it");
    assert_eq!(r.recipes_for_site("a.example", None, now)?.iter().find(|x| x.id == id).unwrap().site_state, Some(SiteState::Active));
    Ok(())
}
