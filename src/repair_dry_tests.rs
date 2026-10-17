//! Tests of the admin's «Test URL» dry run (plan P10.15b). Everything outside is
//! scripted; no network. Pages, names and addresses are invented.

use super::llm::{llm_page, llm_rig, mock, ptr_answer, today};
use super::*;
use crate::repair::dry::{dry_run, DryRunReport};
use crate::repair::AdminTester;
use omni_web::state::{TestUrlError, UrlTester};
use serde_json::Value;

/// The tables a dry run must leave alone (the model's day count is checked apart).
const UNTOUCHED: [&str; 10] = [
    "queue",
    "job_events",
    "repair_attempts",
    "recipes",
    "recipe_sites",
    "recipe_tombstones",
    "recipe_bars",
    "repair_skips",
    "repair_llm_sent",
    "selfheal_reports",
];

/// Every row of every untouched table, as text: an UPDATE shows here as well as an INSERT or DELETE.
fn counts(r: &Rig) -> Vec<String> {
    let conn = rusqlite::Connection::open(r._dir.0.join("omni.db")).unwrap();
    UNTOUCHED
        .iter()
        .map(|t| {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {t} ORDER BY rowid")).unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([], |row| Ok((0..n).map(|i| format!("{:?}", row.get_ref(i).unwrap())).collect::<Vec<_>>().join("|")))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            format!("{t}: {}", rows.join("\n"))
        })
        .collect()
}

/// A connection kept open: its `PRAGMA data_version` changes whenever another connection commits anything.
fn watch(r: &Rig) -> rusqlite::Connection {
    rusqlite::Connection::open(r._dir.0.join("omni.db")).unwrap()
}

fn version(w: &rusqlite::Connection) -> i64 {
    w.query_row("PRAGMA data_version", [], |row| row.get(0)).unwrap()
}

fn in_a_minute() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

fn link(extractor: &str) -> LinkInfo {
    LinkInfo { extractor: extractor.into(), title: Some("Invented story".into()), duration_secs: Some(93.0), formats: 3 }
}

async fn dry(r: &Rig, use_llm: bool) -> DryRunReport {
    dry_run(&r.ctx, PAGE, use_llm, in_a_minute()).await.expect("a public address")
}

fn json_of(report: &DryRunReport) -> Value {
    serde_json::to_value(report).unwrap()
}

#[tokio::test]
async fn the_ladder_part_shows_candidates_with_checks_and_writes_and_downloads_nothing() {
    let r = rig(RepairMode::Hold);
    *r.fake.diagnose_snap.lock().unwrap() = Some(page(&next_data(A)));
    let before = counts(&r);
    assert!(before[0].lines().count() >= 1 && before[0].contains("queue:"), "the dump sees the rig's queued job: {before:?}");
    let w = watch(&r);
    let v0 = version(&w);

    let report = dry(&r, false).await;
    let ladder = report.ladder.as_ref().expect("the normal path fails, so the ladder part runs");
    assert!(ladder.page_looked);
    assert!(ladder.class.is_some());
    let best = &ladder.candidates[0];
    assert_eq!((best.rank, best.verified), (Some(1), true));
    assert!(best.address.starts_with("cdn.news.example.gr/v/a.mp4"), "{}", best.address);
    assert!(best.checks.iter().any(|c| c.name == "probe" && c.ok), "the gate's checks are shown");
    let row = ladder.recipes.iter().find(|x| x.address.is_some()).expect("a recipe found the address");
    assert_eq!(row.gate.as_deref(), Some("verified, rank 1"));
    assert!(report.steps.iter().any(|s| s.name == "Gate") && report.steps.iter().any(|s| s.name == "Recipes"));
    assert!(r.fake.downloads().is_empty(), "never downloads");
    assert_eq!(counts(&r), before, "no job, attempt, event, recipe, bar or skip is written");
    assert_eq!(version(&w), v0, "no commit at all reached the database (an UPDATE too)");
    assert_eq!(r.repo.llm_usage_for(&today()).unwrap().0, 0, "no model call without the tick");

    // Control: the same page through the real ladder does write, so the counter would see a write.
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let res = go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_ne!(counts(&r), before, "the control run wrote an attempt");
    assert_ne!(version(&w), v0, "the control run committed, so the version check can see a write");
}

#[tokio::test]
async fn when_yt_dlp_reads_the_link_the_ladder_does_not_run() {
    let r = rig(RepairMode::Hold);
    *r.fake.ytdl.lock().unwrap() = Some(Ok(link("generic")));
    let report = dry(&r, true).await;
    let y = report.normal.ytdlp.as_ref().unwrap();
    assert_eq!((y.ok, y.extractor.as_deref(), y.formats, y.duration_secs), (true, Some("generic"), 3, Some(93.0)));
    assert!(report.ladder.is_none() && report.normal.sniff.is_none());
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0, "no page look when the normal path works");
    assert_eq!(r.fake.probes(), 0);
}

#[tokio::test]
async fn a_sniffed_stream_is_shown_without_its_query_and_ends_the_test() {
    let r = rig(RepairMode::Hold);
    *r.fake.sniff_seen.lock().unwrap() = Some(SniffSeen {
        stream: Some("https://cdn.news.example.gr/v/a.m3u8?token=SECRETVALUE".into()),
        streams: 2,
        ..Default::default()
    });
    let report = dry(&r, false).await;
    let s = report.normal.sniff.as_ref().unwrap();
    assert_eq!((s.ok, s.streams, s.stream.as_deref()), (true, 2, Some("cdn.news.example.gr/v/a.m3u8")));
    assert!(report.ladder.is_none());
    assert!(!json_of(&report).to_string().contains("SECRETVALUE"), "no query or token in the report");
}

#[tokio::test]
async fn the_model_is_asked_only_when_ticked_and_counts_toward_the_day() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src")]));
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    let before = counts(&r);

    let report = dry(&r, false).await;
    assert!(report.ladder.as_ref().unwrap().llm.is_none());
    assert_eq!((mock(&r).asks(), r.repo.llm_usage_for(&today()).unwrap().0), (0, 0), "not ticked: no call, nothing counted");

    let report = dry(&r, true).await;
    let ladder = report.ladder.as_ref().unwrap();
    let llm = ladder.llm.as_ref().expect("ticked");
    assert_eq!((llm.asked, llm.calls), (true, 1));
    let best = &ladder.candidates[0];
    assert_eq!((best.rank, best.origin.starts_with("llm:")), (Some(1), true));
    assert!(best.address.ends_with("/play/clip42"), "{}", best.address);
    assert_eq!(mock(&r).asks(), 1);
    assert_eq!(r.repo.llm_usage_for(&today()).unwrap(), (1, 1000, 100), "a ticked run counts toward the cap");
    assert_eq!(counts(&r), before, "and writes nothing else, not even the 24 h bundle memory");
    assert!(r.fake.downloads().is_empty());
}

#[tokio::test]
async fn the_daily_cap_stops_a_ticked_run() {
    let mut r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src")]));
    r.ctx.cfg.llm.max_calls_per_day = 1;
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    assert!(r.repo.reserve_llm_call(&today(), 1).unwrap(), "today's only call is used");
    let report = dry(&r, true).await;
    let llm = report.ladder.as_ref().unwrap().llm.as_ref().unwrap();
    assert_eq!((llm.asked, mock(&r).asks()), (false, 0));
    assert!(llm.outcome.contains("not asked"), "{}", llm.outcome);
}

#[tokio::test]
async fn a_model_that_is_not_set_up_is_said_not_called() {
    let r = rig(RepairMode::Hold);
    *r.fake.diagnose_snap.lock().unwrap() = Some(llm_page("Invented story"));
    let report = dry(&r, true).await;
    let llm = report.ladder.as_ref().unwrap().llm.as_ref().unwrap();
    assert!(!llm.asked && llm.outcome.contains("not set up"), "{}", llm.outcome);
}

#[tokio::test]
async fn a_host_that_is_not_public_is_refused_before_anything_runs() {
    let r = rig(RepairMode::Hold);
    r.fake.host_public.store(false, Ordering::SeqCst);
    let err = dry_run(&r.ctx, PAGE, false, in_a_minute()).await.unwrap_err();
    assert!(err.contains("public"), "{err}");
    let err = dry_run(&r.ctx, "http://127.0.0.1/x", false, in_a_minute()).await.unwrap_err();
    assert!(err.contains("private"), "{err}");
    assert_eq!(r.fake.ytdl_calls.load(Ordering::SeqCst), 0);
}

fn tester_for(r: &Rig, budget: Duration) -> Arc<AdminTester> {
    let t = AdminTester::new().with_budget(budget);
    t.set_ctx(Arc::new(RepairCtx { repo: r.repo.clone(), cfg: r.ctx.cfg.clone(), env: r.fake.clone() }));
    Arc::new(t)
}

#[tokio::test]
async fn one_test_runs_at_a_time_and_the_budget_ends_a_hung_tool() {
    let mut f = Fake::new();
    f.ytdl_hangs = true;
    let r = rig_with(RepairMode::Hold, Duration::from_secs(60), f);
    let t = tester_for(&r, Duration::from_secs(1));
    let started = Instant::now();
    let first = {
        let t = t.clone();
        tokio::spawn(async move { t.test_url(PAGE, false).await })
    };
    while r.fake.ytdl_calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(t.test_url(PAGE, false).await.unwrap_err(), TestUrlError::Busy, "a second test while one runs");
    let done = first.await.unwrap().expect("the hung tool is cut at the budget, the report still comes");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(done["normal"]["ytdlp"]["ok"], false);
    // The lock is free again.
    assert!(!matches!(t.test_url(PAGE, false).await, Err(TestUrlError::Busy)));
}

#[tokio::test]
async fn a_tester_without_its_world_refuses_instead_of_running() {
    let t = AdminTester::new();
    assert!(matches!(t.test_url(PAGE, false).await, Err(TestUrlError::Refused(_))));
}

#[tokio::test]
async fn the_site_api_is_asked_first_and_its_video_is_what_yt_dlp_is_asked_about() {
    let r = rig(RepairMode::Hold);
    *r.fake.site_api.lock().unwrap() = Some(Ok(Some("https://cdn.news.example.gr/api/v/9.mp4?token=SECRETVALUE".into())));
    *r.fake.ytdl.lock().unwrap() = Some(Ok(link("generic")));
    let report = dry(&r, false).await;
    let api = report.normal.site_api.as_ref().expect("the API result is shown");
    assert_eq!(api.video.as_deref(), Some("cdn.news.example.gr/api/v/9.mp4"));
    assert_eq!(r.fake.site_api_calls.load(Ordering::SeqCst), 1);
    assert!(report.steps.iter().any(|s| s.name == "Site API" && s.note == "names a video"));
    assert!(!json_of(&report).to_string().contains("SECRETVALUE"));

    // A failed lookup is shown and the usual way goes on.
    *r.fake.site_api.lock().unwrap() = Some(Err("boom: https://api.news.example.gr/x?token=SECRETVALUE".into()));
    let report = dry(&r, false).await;
    let api = report.normal.site_api.as_ref().unwrap();
    assert!(api.video.is_none() && api.error.as_deref().unwrap().contains("api.news.example.gr/x"));
    assert!(!json_of(&report).to_string().contains("SECRETVALUE"));
    assert!(report.normal.ytdlp.as_ref().unwrap().ok);
}

#[tokio::test]
async fn the_ladder_never_runs_for_a_platform_post_and_the_test_says_so() {
    let r = rig(RepairMode::Hold);
    *r.fake.diagnose_snap.lock().unwrap() = Some(page(&next_data(A)));
    let report = dry_run(&r.ctx, "https://www.youtube.com/watch?v=abc", false, in_a_minute()).await.unwrap();
    assert_eq!(report.normal.kind, "platform post");
    assert!(report.ladder.is_none(), "no ladder for a platform post");
    assert!(report.verdict.contains("does not sniff it"), "YouTube is never sniffed: {}", report.verdict);
    assert_eq!((r.fake.diagnose_calls.load(Ordering::SeqCst), r.fake.sniff_calls.load(Ordering::SeqCst), r.fake.probes()), (0, 0, 0));

    // The control: an article page does reach the ladder.
    let report = dry(&r, false).await;
    assert_eq!(report.normal.kind, "web page");
    assert!(report.ladder.is_some());
}

#[tokio::test]
async fn a_sniffed_stream_says_what_the_real_job_would_do_with_it() {
    let r = rig(RepairMode::Hold);
    *r.fake.sniff_seen.lock().unwrap() =
        Some(SniffSeen { stream: Some("https://cdn.news.example.gr/v/a.m3u8".into()), streams: 1, ..Default::default() });
    let report = dry(&r, false).await;
    assert!(report.verdict.contains("would download that stream"), "{}", report.verdict);
    assert!(report.verdict.contains("only if that download failed"), "{}", report.verdict);
    assert!(report.verdict.contains("drops sniffed platform posts"), "{}", report.verdict);
    assert!(report.ladder.is_none());
}

#[tokio::test]
async fn addresses_in_tool_errors_are_cut_to_host_and_path() {
    let r = rig(RepairMode::Hold);
    *r.fake.ytdl.lock().unwrap() = Some(Err("ERROR: [generic] Unsupported URL: https://news.example.gr/story/1?token=SECRETVALUE#x".into()));
    *r.fake.sniff_seen.lock().unwrap() =
        Some(SniffSeen { error: Some("no stream on (http://news.example.gr/a/b?sid=SECRETVALUE).".into()), ..Default::default() });
    let report = dry(&r, false).await;
    let text = json_of(&report).to_string();
    assert!(!text.contains("SECRETVALUE"), "{text}");
    assert_eq!(report.normal.ytdlp.as_ref().unwrap().error.as_deref(), Some("ERROR: [generic] Unsupported URL: news.example.gr/story/1"));
    assert_eq!(report.normal.sniff.as_ref().unwrap().error.as_deref(), Some("no stream on (news.example.gr/a/b)."));
}

#[tokio::test]
async fn a_closed_request_does_not_free_the_lock_until_the_browser_work_has_ended() {
    let mut f = Fake::new();
    f.sniff_delay = Duration::from_millis(2500);
    let r = rig_with(RepairMode::Hold, Duration::from_secs(60), f);
    let t = tester_for(&r, Duration::from_secs(30));
    let request = {
        let t = t.clone();
        tokio::spawn(async move { t.test_url(PAGE, false).await })
    };
    while r.fake.sniff_calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = Instant::now();
    request.abort();
    let _ = request.await;
    assert_eq!(t.test_url(PAGE, false).await.unwrap_err(), TestUrlError::Busy, "the sniff of the closed request still runs");
    assert!(started.elapsed() < Duration::from_secs(2), "checked while the fake was still sleeping");
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(!matches!(t.test_url(PAGE, false).await, Err(TestUrlError::Busy)), "free once the fake ended");
}

#[tokio::test]
async fn a_platform_post_yt_dlp_cannot_read_is_sniffed_but_never_reaches_the_ladder() {
    let r = rig(RepairMode::Hold);
    *r.fake.diagnose_snap.lock().unwrap() = Some(page(&next_data(A)));
    let report = dry_run(&r.ctx, "https://www.instagram.com/reel/abc/", false, in_a_minute()).await.unwrap();
    assert_eq!(report.normal.kind, "platform post");
    assert_eq!(r.fake.sniff_calls.load(Ordering::SeqCst), 1, "after Unsupported URL the real job sniffs a platform post");
    assert!(report.normal.sniff.is_some() && report.ladder.is_none());
    assert!(report.verdict.contains("never runs for this kind of link"), "{}", report.verdict);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_page_that_is_not_found_is_not_sniffed_and_the_ladder_does_not_run() {
    let r = rig(RepairMode::Hold);
    *r.fake.diagnose_snap.lock().unwrap() = Some(page(&next_data(A)));
    *r.fake.ytdl.lock().unwrap() = Some(Err("ERROR: [generic] story: Unable to download webpage: HTTP Error 404: Not Found".into()));
    let report = dry(&r, false).await;
    assert_eq!(report.normal.kind, "web page");
    assert_eq!(r.fake.sniff_calls.load(Ordering::SeqCst), 0, "the real job does not sniff a 404");
    assert!(report.normal.sniff.is_none() && report.ladder.is_none());
    assert!(report.verdict.contains("does not sniff it") && report.verdict.contains("PAGE_NOT_FOUND"), "{}", report.verdict);
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);

    // The control: with another failure the same page is sniffed and reaches the ladder.
    *r.fake.ytdl.lock().unwrap() = Some(Err("ERROR: Unsupported URL: https://news.example.gr/story/1".into()));
    let report = dry(&r, false).await;
    assert_eq!(r.fake.sniff_calls.load(Ordering::SeqCst), 1);
    assert!(report.ladder.is_some());
}
