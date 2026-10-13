//! Tests of the overnight self-heal (plan P10.14): the probe-only ladder on a broken
//! site's canary pages, and its scheduling. Everything outside is scripted; no network,
//! nothing is ever downloaded. Pages, names and addresses are invented.

use super::llm::{click_answer, good_recipe, held_once, llm_on, llm_page, llm_recipes, llm_rig_with, mock, ptr_answer, wrong_recipe, CLIP};
use super::*;
use crate::canary::{self, CanaryEnv};
use omni_core::health::HealthState;
use omni_core::recipe_score::{InsertOutcome, NewRecipe, Origin, ScopeKind, SiteState};
use omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS;
use omni_core::site_health::{SiteOutcome, SiteState as HealthSiteState};
use serde_json::{json, Value};

/// A recipe that reads the `data-clip` attribute of the page's `div`.
const CLIP_RECIPE: &str = r#"{"dsl_version":1,"steps":[{"op":"attr","css":"div[data-clip]","attr":"data-clip"},{"op":"abs","from":0},{"op":"media","from":1,"referer":"page"}]}"#;

fn clip_page(url: &str) -> PageSnapshot {
    page(&format!(r#"<html><body><div data-clip="{url}"></div></body></html>"#))
}

/// Store the clip recipe for the page's site; the new recipe is a candidate there.
fn clip_recipe(r: &Rig) -> i64 {
    let rec = Recipe::from_json(CLIP_RECIPE).unwrap();
    let new = NewRecipe {
        scope_kind: ScopeKind::Site,
        scope_key: DOMAIN.to_string(),
        family: None,
        recipe_json: rec.canonical_json(),
        hash: recipe::scope_hash("site", DOMAIN, &rec),
        steps_hash: recipe::steps_hash(&rec),
        dsl_version: rec.dsl_version,
        origin: Origin::Admin,
    };
    match r.repo.insert_recipe(&new, Utc::now()).unwrap() {
        InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) => id,
        other => panic!("recipe not stored: {other:?}"),
    }
}

fn suspend_failing(r: &Rig, id: i64) {
    for _ in 0..3 {
        r.repo.recipe_event(id, DOMAIN, Event::NoCandidate, Utc::now()).unwrap();
    }
}

/// The recipe's state on the site, as the self-heal sees the store.
fn state_of(r: &Rig, id: i64) -> Option<SiteState> {
    r.repo.recipes_for_canary(DOMAIN, None, Utc::now()).unwrap().into_iter().find(|x| x.id == id).and_then(|x| x.site_state)
}

fn offered(r: &Rig, id: i64) -> bool {
    r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap().iter().any(|x| x.id == id)
}

/// Whether one more canary pass would be the second in a row: apply it and read the state.
/// (Used to tell "no pass was recorded" from "one was".)
fn one_more_pass_activates(r: &Rig, id: i64) -> bool {
    r.repo.recipe_event(id, DOMAIN, Event::CanaryPass, Utc::now()).unwrap();
    state_of(r, id) == Some(SiteState::Active)
}

/// A finished job of the site that delivered `secs` seconds; `0.0`: not known.
fn delivered_job(r: &Rig, secs: f64) -> i64 {
    let id = r
        .repo
        .enqueue(&NewJob::new("https://news.example.gr/story/old", "9_ANNA_OLD", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    r.repo.update_job_status(id, JobStatus::Completed, None, None, (secs > 0.0).then_some(secs)).unwrap();
    id
}

/// `n` canary pages of the site whose original job delivered `secs` seconds.
fn canary_pages(r: &Rig, n: usize, secs: f64) -> Vec<String> {
    let job = delivered_job(r, secs);
    (0..n)
        .map(|i| {
            let url = format!("https://news.example.gr/story/c{i}");
            assert!(r.repo.add_canary(DOMAIN, &url, Some(job), Utc::now()).unwrap());
            url
        })
        .collect()
}

async fn heal(r: &Rig, pages: &[String]) -> SiteHeal {
    probe_site(&r.ctx, DOMAIN, pages, Utc::now(), Instant::now() + Duration::from_secs(120)).await
}

fn serve_clip(r: &Rig) {
    *r.fake.diagnose_snap.lock().unwrap() = Some(clip_page(A));
}

const CLIPS: [&str; 3] = [A, B, C];

/// The first `n` canary pages each show a different video (one video on every page is one page).
fn serve_clips(r: &Rig, n: usize) {
    r.fake.diagnose_script.lock().unwrap().extend(CLIPS[..n].iter().map(|u| Some(clip_page(u))));
}

/// The rig's own job is RUNNING; the nightly heal waits for an idle queue.
fn idle(r: &Rig) {
    assert!(r.repo.finish(r.job.id, OWNER, JobStatus::Completed, None, None, None).unwrap());
}

/// `llm_page` whose script video is `clip4{k}`: another page, another video, another bundle.
fn llm_page_k(k: usize) -> PageSnapshot {
    let mut s = llm_page(&format!("Invented story {k}"));
    s.scripts = s.scripts.iter().map(|t| t.replace("clip42", &format!("clip4{k}"))).collect();
    s
}

// ---- acceptance: a template change, healed overnight, then the next job needs no model ----

#[tokio::test]
async fn acceptance_a_changed_site_is_healed_overnight_and_the_next_job_is_repaired_by_the_recipe_with_no_model_call() {
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": good_recipe().to_string()});
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(60), Fake::new(), MockModel::new(vec![answer]));
    r.fake.diagnose_script.lock().unwrap().extend([Some(llm_page_k(2)), Some(llm_page_k(3)), Some(llm_page_k(4))]);
    let pages = canary_pages(&r, 3, 120.0);

    // The site's canaries fail on the normal path; the night's probe asks the model once, on the first page.
    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "found", "{night:?}");
    let found = night.found.expect("a recipe was found");
    assert_eq!((found.origin.as_str(), found.pages), ("llm", 3), "the model's recipe worked on all three canary pages");
    assert_eq!(mock(&r).asks(), 1, "one model call for the whole site");
    assert!(r.fake.downloads().is_empty(), "the night downloads nothing");
    let stored = llm_recipes(&r);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].site_state, Some(SiteState::Active), "two canary passes made it active");
    assert_eq!(found.recipe_id, stored[0].id);

    // The next job on the site is repaired on the recipe rung with no model call, and held for MCR.
    mock(&r).panics.store(true, Ordering::SeqCst);
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(mock(&r).asks(), 1, "no further model call");
    let d = r.fake.downloads();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].0, CLIP);
    assert_eq!(d[0].2, DeliveryMode::Hold { hold_hours: 24, held_max_bytes: 20 << 30 }, "still waits for MCR");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.rung.as_deref(), at.llm_calls, at.outcome.as_deref()), (Some("recipes"), 0, Some("held")));
    assert_eq!(at.recipe_id, Some(found.recipe_id), "the attempt names the healed recipe");
}

// ---- a suspended recipe earns its way back ----

#[tokio::test]
async fn a_recipe_suspended_as_failing_that_works_again_on_two_canary_pages_is_active_again() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    suspend_failing(&r, id);
    assert!(!offered(&r, id), "suspended: the job path does not offer it");
    serve_clips(&r, 3);
    let pages = canary_pages(&r, 3, 120.0);

    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "found", "{night:?}");
    assert_eq!(night.found.as_ref().map(|f| (f.recipe_id, f.pages)), Some((id, 3)));
    assert_eq!(state_of(&r, id), Some(SiteState::Active));
    assert!(offered(&r, id), "recipes_for_site offers it again");
}

#[tokio::test]
async fn a_recipe_suspended_as_rejected_is_never_tried() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    r.repo.recipe_event(id, DOMAIN, Event::Rejection, Utc::now()).unwrap();
    // The rejection also bars the recipe's steps; delete that bar (bars expire on their own)
    // so that only the recipe's own suspension as rejected is left to keep it out.
    let db = r._dir.0.join("omni.db");
    rusqlite::Connection::open(db).unwrap().execute("DELETE FROM recipe_bars", []).unwrap();
    let steps = recipe::steps_hash(&Recipe::from_json(CLIP_RECIPE).unwrap());
    assert!(!r.repo.recipe_steps_barred(&steps, DOMAIN).unwrap(), "the bar is gone");
    serve_clips(&r, 3);
    let pages = canary_pages(&r, 3, 120.0);

    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "nothing", "{night:?}");
    assert!(night.found.is_none());
    assert_eq!(r.fake.probes(), 0, "its video was never even looked at");
    assert_eq!(state_of(&r, id), None, "a rejected recipe is not in the canary list");
    assert!(!offered(&r, id));
    let row = r.repo.recipe_event(id, DOMAIN, Event::NoCandidate, Utc::now()).unwrap();
    assert_eq!((row.rejections, row.consecutive_fail), (1, 1), "no canary event touched it (the 1 is this call)");
}

#[tokio::test]
async fn an_address_mcr_called_a_wrong_video_is_never_tried_and_gives_no_event() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    serve_clip(&r);
    mcr_calls_wrong(&r.repo, 1, &format!("{A}?sig=1"));
    let pages = canary_pages(&r, 3, 120.0);

    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "nothing", "{night:?}");
    assert_eq!(r.fake.probes(), 0, "the skipped address never reached the gate");
    assert_eq!(state_of(&r, id), None, "no event: it has no row on the site");
    assert!(!one_more_pass_activates(&r, id), "and no pass was recorded");
}

/// MCR rejected the video of this recipe as the wrong one: the recipe is suspended as
/// rejected and its steps are barred on the site.
fn mcr_rejects_recipe(r: &Rig, id: i64, hash: &str) {
    let job = r
        .repo
        .enqueue(&NewJob::new("https://news.example.gr/other-9", "9_PETROU_X", "PETROU"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    assert!(r.repo.lease_job_by_id(job, "desk-test", 600).unwrap());
    let at = r.repo.start_attempt(job, "hold", Utc::now()).unwrap();
    let end = AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        domain: Some(DOMAIN.into()),
        recipe_id: Some(id),
        recipe_hash: Some(hash.to_string()),
        held_url: Some(B.to_string()),
        candidates_json: Some(json!([{"url": B}]).to_string()),
        ..Default::default()
    };
    r.repo.finish_attempt(at, &end, Utc::now()).unwrap();
    assert!(r.repo.hold_output(job, "desk-test", "temp/held/x/clip.mxf", 3, &"ab".repeat(32), Some(9.0), Utc::now() + chrono::Duration::hours(24)).unwrap());
    assert!(r.repo.finish(job, "desk-test", JobStatus::AwaitingApproval, None, None, None).unwrap());
    assert!(r.repo.reject_held(job, "wrong_video", "no", "desk").unwrap());
}

#[tokio::test]
async fn steps_mcr_barred_on_the_site_are_never_tried() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    serve_clip(&r);
    let rec = Recipe::from_json(CLIP_RECIPE).unwrap();
    mcr_rejects_recipe(&r, id, &recipe::scope_hash("site", DOMAIN, &rec));
    assert!(r.repo.recipe_steps_barred(&recipe::steps_hash(&rec), DOMAIN).unwrap(), "MCR's rejection bars the steps");
    let pages = canary_pages(&r, 2, 120.0);
    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "nothing", "{night:?}");
    assert_eq!(r.fake.probes(), 0, "a barred recipe never produced a candidate");
    assert_eq!(state_of(&r, id), None);
}

// ---- how many pages count ----

#[tokio::test]
async fn one_counted_page_records_no_pass_and_leaves_the_recipe_as_it_was() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    // The first look is complete, the second never is: one counted page.
    r.fake.diagnose_script.lock().unwrap().extend([Some(clip_page(A)), None]);
    let pages = canary_pages(&r, 2, 120.0);

    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "nothing", "{night:?}");
    assert!(night.found.is_none());
    assert_eq!(state_of(&r, id), None, "the recipe has no row on the site");
    assert!(!one_more_pass_activates(&r, id), "no pass was recorded for the single counted page");
}

#[tokio::test]
async fn two_counted_pages_record_a_pass_each() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    serve_clips(&r, 2);
    let pages = canary_pages(&r, 2, 120.0);
    assert_eq!(heal(&r, &pages).await.outcome, "found");
    assert_eq!(state_of(&r, id), Some(SiteState::Active), "two passes promoted the candidate");
}

#[tokio::test]
async fn a_conclusive_miss_on_each_page_is_a_canary_fail_and_suspends_the_recipe() {
    let fake = Fake::new();
    for u in CLIPS {
        fake.probe.lock().unwrap().insert(u.to_string(), Err("ffprobe could not read the address".to_string()));
    }
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(60), fake);
    let id = clip_recipe(&r);
    serve_clips(&r, 3);
    let pages = canary_pages(&r, 3, 120.0);

    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "nothing", "{night:?}");
    assert_eq!(state_of(&r, id), Some(SiteState::Suspended), "three misses in a row suspend it, as three failed jobs would");
}

#[tokio::test]
async fn an_inconclusive_night_records_nothing() {
    // No page look is ever complete: nothing is known about any recipe.
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    let pages = canary_pages(&r, 3, 120.0);
    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "skipped", "{night:?}");
    assert_eq!(state_of(&r, id), None, "untouched");
    assert_eq!(
        r.repo.recipes_for_canary(DOMAIN, None, Utc::now()).unwrap().iter().filter(|x| x.site_state.is_some()).count(),
        0,
        "no recipe has any row"
    );

    // A gate that ran out of time (the probe hangs past the budget) is no miss either.
    let fake = Fake { probe_delay: Duration::from_secs(5), ..Fake::new() };
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(60), fake);
    let id = clip_recipe(&r);
    serve_clip(&r);
    let pages = canary_pages(&r, 2, 120.0);
    let short = probe_site(&r.ctx, DOMAIN, &pages, Utc::now(), Instant::now() + Duration::from_millis(600)).await;
    assert_ne!(short.outcome, "found", "{short:?}");
    assert_eq!(state_of(&r, id), None, "a timeout says nothing about the recipe");
}

// ---- the length must match ----

#[tokio::test]
async fn a_video_of_the_wrong_length_is_not_counted() {
    let fake = Fake::new();
    for u in CLIPS {
        fake.probe.lock().unwrap().insert(u.to_string(), Ok(info(300.0)));
    }
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(60), fake);
    let id = clip_recipe(&r);
    serve_clips(&r, 3);
    let pages = canary_pages(&r, 3, 120.0);
    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "nothing", "{night:?}: 300 s against the 120 s the canary delivered");
    assert_eq!(state_of(&r, id), None);
    assert!(!one_more_pass_activates(&r, id), "no pass for a video of the wrong length");
}

#[tokio::test]
async fn the_length_tolerance_is_the_larger_of_three_seconds_and_five_percent() {
    let at = |probe: f64, delivered: f64| {
        let v = Verified {
            candidate: Candidate { url: A.into(), referer: None, origin: "recipe:x".into(), position: None, in_article: None },
            duration_secs: probe,
            height: 720,
            kind: omni_broadcast::repair_gate::ProbeKind::Direct,
            checks: vec![],
            score: 0,
        };
        duration_counts(&v, Some(delivered))
    };
    // 5 % of 40 s is 2 s: the 3 s floor applies. 5 % of 200 s is 10 s: the percentage applies.
    assert!(at(42.9, 40.0) && at(37.1, 40.0), "within 3 s of a short video");
    assert!(!at(43.5, 40.0) && !at(36.5, 40.0), "just outside the 3 s floor");
    assert!(at(209.5, 200.0) && at(190.5, 200.0), "within 5 % of a long video");
    assert!(!at(211.0, 200.0) && !at(188.0, 200.0), "just outside 5 %");
}

#[tokio::test]
async fn with_no_known_length_only_a_page_stated_matching_length_counts() {
    // Unknown on both sides: not counted.
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    serve_clip(&r);
    let pages = canary_pages(&r, 2, 0.0);
    assert_eq!(heal(&r, &pages).await.outcome, "nothing");
    assert!(!one_more_pass_activates(&r, id));

    // The page states the length and the probe matches it: counted.
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    for u in [A, B] {
        let mut snap = clip_page(u);
        snap.meta.push(("og:video:duration".into(), "120".into()));
        r.fake.diagnose_script.lock().unwrap().push_back(Some(snap));
    }
    let pages = canary_pages(&r, 2, 0.0);
    assert_eq!(heal(&r, &pages).await.outcome, "found");
    assert_eq!(state_of(&r, id), Some(SiteState::Active));

    // The page states a length the video does not have: the gate rejects it, nothing counts.
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    let mut snap = clip_page(A);
    snap.meta.push(("og:video:duration".into(), "30".into()));
    *r.fake.diagnose_snap.lock().unwrap() = Some(snap);
    let pages = canary_pages(&r, 2, 0.0);
    assert_ne!(heal(&r, &pages).await.outcome, "found");
    assert_ne!(state_of(&r, id), Some(SiteState::Active));
}

// ---- probe only ----

#[tokio::test]
async fn a_probe_never_downloads_holds_or_creates_a_job_or_an_attempt() {
    let r = rig(RepairMode::Hold);
    let id = clip_recipe(&r);
    serve_clips(&r, 3);
    let pages = canary_pages(&r, 3, 120.0);
    let jobs_before = r.repo.get_jobs_by_status(JobStatus::Pending).unwrap().len() + r.repo.get_jobs_by_status(JobStatus::Completed).unwrap().len();

    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "found", "{night:?}");
    assert_eq!(state_of(&r, id), Some(SiteState::Active));
    assert!(r.fake.downloads().is_empty(), "nothing was downloaded");
    assert!(r.repo.latest_attempt(r.job.id).unwrap().is_none(), "no repair attempt");
    let jobs_after = r.repo.get_jobs_by_status(JobStatus::Pending).unwrap().len() + r.repo.get_jobs_by_status(JobStatus::Completed).unwrap().len();
    assert_eq!(jobs_before, jobs_after, "no job was created");
    assert!(r.repo.waiting_held(r.job.id).unwrap().is_none(), "nothing held");
    let leased = r.repo.get_job(r.job.id).unwrap().unwrap();
    assert_eq!(leased.status, JobStatus::Running, "an unrelated job is not touched");
    assert_eq!(leased.extraction_method, None);
}

#[tokio::test]
async fn at_most_three_pages_are_looked_at_and_repair_off_probes_nothing() {
    let r = rig(RepairMode::Shadow);
    clip_recipe(&r);
    serve_clip(&r);
    let pages: Vec<String> = (0..5).map(|i| format!("https://news.example.gr/story/p{i}")).collect();
    heal(&r, &pages).await;
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 3);

    let r = rig(RepairMode::Off);
    serve_clip(&r);
    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "skipped");
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);
}

// ---- the model rung ----

#[tokio::test]
async fn the_models_daily_cap_and_the_site_breaker_apply_to_the_probe() {
    // Cap: zero calls a day.
    let mut r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new(), MockModel::new(vec![]));
    r.ctx.cfg.llm.max_calls_per_day = 0;
    mock(&r).panics.store(true, Ordering::SeqCst);
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    let pages = canary_pages(&r, 2, 120.0);
    assert_ne!(heal(&r, &pages).await.outcome, "found");
    assert_eq!(mock(&r).asks(), 0, "the day's cap is respected");

    // Breaker: open for the site.
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new(), MockModel::new(vec![]));
    mock(&r).panics.store(true, Ordering::SeqCst);
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    r.repo.record_site_outcome(DOMAIN, SiteOutcome::Success).unwrap();
    r.repo.open_llm_breaker(DOMAIN, Utc::now() + chrono::Duration::hours(24)).unwrap();
    let pages = canary_pages(&r, 2, 120.0);
    assert_ne!(heal(&r, &pages).await.outcome, "found");
    assert_eq!(mock(&r).asks(), 0, "the site's breaker is respected");

    // Not enabled: no model even with one present.
    let mut r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new(), MockModel::new(vec![]));
    r.ctx.cfg.llm = LlmCfg::default();
    mock(&r).panics.store(true, Ordering::SeqCst);
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    let pages = canary_pages(&r, 2, 120.0);
    assert_ne!(heal(&r, &pages).await.outcome, "found");
    assert_eq!(llm_on().max_calls_per_day, 15);
}

#[tokio::test]
async fn a_model_recipe_that_does_not_reproduce_the_address_is_never_stored_by_a_probe() {
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": wrong_recipe().to_string()});
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new(), MockModel::new(vec![answer]));
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    let pages = canary_pages(&r, 2, 120.0);
    let night = heal(&r, &pages).await;
    assert_ne!(night.outcome, "found", "{night:?}");
    assert!(llm_recipes(&r).is_empty(), "a pointer without a reproducing recipe is no recipe");
}

#[tokio::test]
async fn a_model_video_of_the_wrong_length_is_not_stored_as_a_recipe() {
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": good_recipe().to_string()});
    let fake = Fake::new();
    fake.probe.lock().unwrap().insert(CLIP.to_string(), Ok(info(400.0)));
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), fake, MockModel::new(vec![answer]));
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    let pages = canary_pages(&r, 2, 120.0);
    assert_ne!(heal(&r, &pages).await.outcome, "found");
    assert!(llm_recipes(&r).is_empty(), "no hold vouches for it, so the length must");
}

// ---- scheduling ----

fn canary_env(r: &Rig) -> Arc<CanaryEnv> {
    Arc::new(CanaryEnv::new(r.repo.clone(), HealthState::new(), PathBuf::from("bin"), "stable".into()))
}

fn broken(r: &Rig) {
    r.repo.record_site_outcome(DOMAIN, SiteOutcome::Success).unwrap();
    assert!(r.repo.set_site_state(DOMAIN, HealthSiteState::Broken).unwrap());
}

fn reports(r: &Rig) -> Vec<omni_core::repository::SelfhealReport> {
    r.repo.selfheal_since(Utc::now() - chrono::Duration::hours(1)).unwrap()
}

#[tokio::test]
async fn the_nightly_heal_probes_only_broken_sites_and_reports_each() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    serve_clips(&r, 3);
    canary_pages(&r, 3, 120.0);
    idle(&r);
    let env = canary_env(&r);

    // Degraded is not broken: nothing runs.
    r.repo.record_site_outcome(DOMAIN, SiteOutcome::Success).unwrap();
    assert!(r.repo.set_site_state(DOMAIN, HealthSiteState::Degraded).unwrap());
    assert_eq!(canary::heal_nightly_with(&env, &r.ctx).await, 0);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);

    broken(&r);
    assert_eq!(canary::heal_nightly_with(&env, &r.ctx).await, 1);
    assert_eq!(state_of(&r, id), Some(SiteState::Active));
    let rows = reports(&r);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].domain.as_str(), rows[0].outcome.as_str(), rows[0].pages), (DOMAIN, "found", 3));
    assert_eq!(rows[0].recipe_hash.as_deref().map(str::len), Some(64), "the full recipe hash, never a page address");
    assert!(rows[0].detail.as_deref().unwrap_or("").is_empty());
}

#[tokio::test]
async fn with_repair_off_the_nightly_heal_never_runs() {
    let r = rig(RepairMode::Off);
    clip_recipe(&r);
    serve_clip(&r);
    canary_pages(&r, 3, 120.0);
    broken(&r);
    let env = canary_env(&r);
    assert_eq!(canary::heal_nightly_with(&env, &r.ctx).await, 0);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);
    assert!(reports(&r).is_empty());
}

#[tokio::test]
async fn after_an_outage_pass_the_nightly_heal_never_runs() {
    let r = rig(RepairMode::Shadow);
    clip_recipe(&r);
    serve_clip(&r);
    canary_pages(&r, 3, 120.0);
    broken(&r);
    idle(&r);
    let env = canary_env(&r);
    env.mark_outage(true);
    assert_eq!(canary::heal_nightly_with(&env, &r.ctx).await, 0);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0, "nothing was looked at");
    let rows = reports(&r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "outage");
    assert!(rows[0].recipe_hash.is_none());
}

#[tokio::test]
async fn the_daytime_heal_waits_for_an_idle_queue_and_runs_once_per_two_hours() {
    let r = rig(RepairMode::Shadow);
    clip_recipe(&r);
    serve_clip(&r);
    canary_pages(&r, 3, 120.0);
    broken(&r);
    let env = canary_env(&r);

    // The rig's job is RUNNING: not now.
    assert!(!canary::heal_by_day(&env, &r.ctx, DOMAIN).await);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);
    assert!(reports(&r).is_empty());

    // Nothing running: it runs once, then not again within 2 hours.
    assert!(r.repo.finish(r.job.id, OWNER, JobStatus::Completed, None, None, None).unwrap());
    assert!(canary::heal_by_day(&env, &r.ctx, DOMAIN).await);
    assert_eq!(reports(&r).len(), 1);
    let looked = r.fake.diagnose_calls.load(Ordering::SeqCst);
    assert_eq!(looked, 3);
    assert!(!canary::heal_by_day(&env, &r.ctx, DOMAIN).await, "the same site within 2 hours");
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), looked);

    // Repair off: never.
    let r = rig(RepairMode::Off);
    assert!(r.repo.finish(r.job.id, OWNER, JobStatus::Completed, None, None, None).unwrap());
    canary_pages(&r, 3, 120.0);
    assert!(!canary::heal_by_day(&canary_env(&r), &r.ctx, DOMAIN).await);
}

#[tokio::test]
async fn the_nightly_heal_waits_for_the_canary_lock() {
    let r = rig(RepairMode::Shadow);
    clip_recipe(&r);
    serve_clip(&r);
    canary_pages(&r, 3, 120.0);
    broken(&r);
    idle(&r);
    let env = canary_env(&r);

    // The 03:00 yt-dlp update holds the lock.
    let guard = env.lock_run().await;
    let task = {
        let env = env.clone();
        let ctx = RepairCtx { repo: r.repo.clone(), cfg: r.ctx.cfg.clone(), env: r.ctx.env.clone() };
        tokio::spawn(async move { canary::heal_nightly_with(&env, &ctx).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "the heal waits while the updater holds the lock");
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0, "no page was looked at meanwhile");
    drop(guard);
    assert_eq!(task.await.unwrap(), 1);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_site_with_no_live_canary_pages_is_not_probed() {
    let r = rig(RepairMode::Shadow);
    clip_recipe(&r);
    serve_clip(&r);
    broken(&r);
    let env = canary_env(&r);
    assert_eq!(canary::heal_nightly_with(&env, &r.ctx).await, 0);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);
}

// ---- review fixes ----

#[tokio::test]
async fn a_hit_a_miss_and_a_hit_records_only_the_fail_and_reports_nothing_found() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    // Page 1 and 3 show a clip; page 2 has none for the recipe to read.
    r.fake.diagnose_script.lock().unwrap().extend([Some(clip_page(A)), Some(blank()), Some(clip_page(C))]);
    let pages = canary_pages(&r, 3, 120.0);

    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "nothing", "{night:?}");
    assert!(night.found.is_none(), "two hits around a miss are not a recipe that works");
    // Only the fail was recorded (no passes): one more pass is the first of a streak, not a second.
    assert!(!one_more_pass_activates(&r, id));
    let row = r.repo.recipe_event(id, DOMAIN, Event::CanaryPass, Utc::now()).unwrap();
    assert!(row.alpha < 2.2, "no pass was added to the score by the night: alpha {}", row.alpha);
}

#[tokio::test]
async fn an_already_active_recipe_that_verifies_again_gets_its_passes_but_is_not_reported_found() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    r.repo.recipe_event(id, DOMAIN, Event::CanaryPass, Utc::now()).unwrap();
    r.repo.recipe_event(id, DOMAIN, Event::CanaryPass, Utc::now()).unwrap();
    assert_eq!(state_of(&r, id), Some(SiteState::Active), "active before the night");
    serve_clips(&r, 3);
    let pages = canary_pages(&r, 3, 120.0);

    let night = heal(&r, &pages).await;
    assert!(night.found.is_none(), "{night:?}: nothing new was found");
    assert_eq!(night.outcome, "nothing");
    let row = r.repo.recipe_event(id, DOMAIN, Event::CanaryPass, Utc::now()).unwrap();
    // 2 manual passes (alpha 2.0), the night's 3 (1.5 more), this one (0.5 more).
    assert!(row.alpha > 3.9, "the night's passes were recorded: alpha {}", row.alpha);
}

#[tokio::test]
async fn the_same_video_on_every_page_counts_as_one_page() {
    let r = rig(RepairMode::Shadow);
    let id = clip_recipe(&r);
    serve_clip(&r);
    let pages = canary_pages(&r, 3, 120.0);
    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "nothing", "{night:?}: a sidebar clip is not proof");
    assert_eq!(state_of(&r, id), None);
    assert!(!one_more_pass_activates(&r, id), "no pass was recorded");
}

#[tokio::test]
async fn only_the_first_page_without_a_recipe_hit_gets_the_model_and_the_run_has_a_call_budget() {
    // Three pages, none solved by a recipe: the model is asked about the first only.
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new(), MockModel::new(vec![]));
    r.fake.diagnose_script.lock().unwrap().extend([Some(llm_page_k(2)), Some(llm_page_k(3)), Some(llm_page_k(4))]);
    let pages = canary_pages(&r, 3, 120.0);
    assert_ne!(heal(&r, &pages).await.outcome, "found");
    assert_eq!(mock(&r).asks(), 1, "one page per site and run");

    // The run's calls are shared by its sites: none left, none asked.
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new(), MockModel::new(vec![]));
    mock(&r).panics.store(true, Ordering::SeqCst);
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page_k(2));
    let pages = canary_pages(&r, 2, 120.0);
    let none_left = AtomicU32::new(0);
    probe_site_with(&r.ctx, DOMAIN, &pages, Utc::now(), Instant::now() + Duration::from_secs(120), &none_left).await;
    assert_eq!(mock(&r).asks(), 0);

    // Calls spent come off the run's budget.
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new(), MockModel::new(vec![]));
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page_k(2));
    let pages = canary_pages(&r, 2, 120.0);
    let left = AtomicU32::new(PROBE_LLM_CALLS);
    probe_site_with(&r.ctx, DOMAIN, &pages, Utc::now(), Instant::now() + Duration::from_secs(120), &left).await;
    assert_eq!(mock(&r).asks(), 1);
    assert_eq!(left.load(Ordering::SeqCst), PROBE_LLM_CALLS - 1);
    assert_eq!(PROBE_LLM_CALLS, 5);
}

#[tokio::test]
async fn the_nightly_heal_skips_a_night_with_a_job_running_and_writes_no_report() {
    let r = rig(RepairMode::Shadow);
    clip_recipe(&r);
    serve_clips(&r, 3);
    canary_pages(&r, 3, 120.0);
    broken(&r);
    let env = canary_env(&r);
    // The rig's job is RUNNING (a late start beside live work).
    assert_eq!(canary::heal_nightly_with(&env, &r.ctx).await, 0);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);
    assert!(reports(&r).is_empty());
    idle(&r);
    assert_eq!(canary::heal_nightly_with(&env, &r.ctx).await, 1, "the next quiet run does it");
}

/// A page whose script holds three different video addresses (r1, r2, r3), and a fake that
/// rejects all three at the gate: a model pointing at them keeps going for every round it gets.
fn three_rejected(model_answers: usize) -> Rig {
    let answers: Vec<Value> = ["/r1", "/r2", "/r3"].iter().take(model_answers).map(|p| ptr_answer(p)).collect();
    let fake = Fake::new();
    for k in 1..=3 {
        fake.probe.lock().unwrap().insert(format!("https://cdn.news.example.gr/play/r{k}"), Err("ffprobe could not read the address".to_string()));
    }
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), fake, MockModel::new(answers));
    let mut snap = llm_page("Invented story");
    snap.scripts = vec![r#"{"r1":"https:\/\/cdn.news.example.gr\/play\/r1","r2":"https:\/\/cdn.news.example.gr\/play\/r2","r3":"https:\/\/cdn.news.example.gr\/play\/r3"}"#.into()];
    *r.fake.diagnose_snap.lock().unwrap() = Some(snap);
    r
}

#[tokio::test]
async fn a_daytime_heal_makes_at_most_one_model_call() {
    let r = three_rejected(3);
    canary_pages(&r, 3, 120.0);
    broken(&r);
    idle(&r);
    let env = canary_env(&r);
    assert!(canary::heal_by_day(&env, &r.ctx, DOMAIN).await);
    assert_eq!(mock(&r).asks(), 1, "one call for the heal, though the model would go three rounds");
    assert_eq!(env.day_llm_used(), 1, "and it is counted for the day");
    assert_eq!(DAY_PROBE_LLM_CALLS, 1);
}

#[tokio::test]
async fn the_rounds_of_a_probe_never_exceed_the_calls_left_to_the_run() {
    // Uncapped the same rig asks three times.
    let r = three_rejected(3);
    let pages = canary_pages(&r, 2, 120.0);
    let left = AtomicU32::new(2);
    probe_site_with(&r.ctx, DOMAIN, &pages, Utc::now(), Instant::now() + Duration::from_secs(120), &left).await;
    assert_eq!(mock(&r).asks(), 2usize.min(LLM_MAX_ROUNDS), "min(calls left, 3) rounds");
    assert_eq!(left.load(Ordering::SeqCst), 0);

    let r = three_rejected(3);
    let pages = canary_pages(&r, 2, 120.0);
    let left = AtomicU32::new(PROBE_LLM_CALLS);
    probe_site_with(&r.ctx, DOMAIN, &pages, Utc::now(), Instant::now() + Duration::from_secs(120), &left).await;
    assert_eq!(mock(&r).asks(), LLM_MAX_ROUNDS, "five left: still three rounds at most");
    assert_eq!(left.load(Ordering::SeqCst), PROBE_LLM_CALLS - 3);
}

#[tokio::test]
async fn a_capped_probe_never_does_a_live_load_in_its_last_allowed_round() {
    let fake = Fake::new();
    set_live(&fake, llm_page("Clicked"), LiveRun::Complete, &["clicked"]);
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(120), fake, MockModel::new(vec![click_answer(), ptr_answer("/video/src")]));
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    let pages = canary_pages(&r, 2, 120.0);
    let left = AtomicU32::new(1);
    probe_site_with(&r.ctx, DOMAIN, &pages, Utc::now(), Instant::now() + Duration::from_secs(120), &left).await;
    assert_eq!(mock(&r).asks(), 1);
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 0, "the click would need a second round the probe does not have");
}

#[tokio::test]
async fn addresses_that_differ_only_in_the_query_count_as_two_pages_when_their_lengths_differ() {
    let fake = Fake::new();
    fake.probe.lock().unwrap().insert(format!("{A}?x=1"), Ok(info(114.5)));
    fake.probe.lock().unwrap().insert(format!("{A}?x=2"), Ok(info(125.5)));
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(60), fake);
    let id = clip_recipe(&r);
    r.fake.diagnose_script.lock().unwrap().extend([Some(clip_page(&format!("{A}?x=1"))), Some(clip_page(&format!("{A}?x=2")))]);
    let pages = canary_pages(&r, 2, 120.0);
    let night = heal(&r, &pages).await;
    assert_eq!(night.outcome, "found", "{night:?}: same path, but 11 s apart: two videos");
    assert_eq!(state_of(&r, id), Some(SiteState::Active));
}

#[tokio::test]
async fn all_daytime_heals_of_a_day_together_stop_at_three_model_calls() {
    let r = llm_rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new(), MockModel::new(vec![]));
    mock(&r).panics.store(true, Ordering::SeqCst);
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page_k(2));
    canary_pages(&r, 2, 120.0);
    broken(&r);
    idle(&r);
    let env = canary_env(&r);
    env.set_day_llm_used_for_tests(3);
    assert!(canary::heal_by_day(&env, &r.ctx, DOMAIN).await, "the heal itself still runs");
    assert_eq!(mock(&r).asks(), 0, "the day's three calls are used up: no model call");
    assert_eq!(env.day_llm_used(), 3);
}
