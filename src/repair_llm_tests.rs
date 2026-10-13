//! Tests of the ladder's LLM rung (plan P10.12). The model is a scripted mock behind
//! `RepairModel`; the real client is only used against a local socket. No network.
//! Pages, names and addresses are invented.

use super::*;
use serde_json::{json, Value};

const CLIP: &str = "https://cdn.news.example.gr/play/clip42";
/// The page's own network entry N1 (the only one), a host the page used.
const OTHER: &str = "https://cdn.news.example.gr/play/other77";
const CLIP99: &str = "https://cdn.news.example.gr/play/clip99";

/// A page whose only video address sits in a JSON script window (S1), without a
/// `.mp4`/`.m3u8` ending, so no shipped recipe finds it.
fn llm_page(title: &str) -> PageSnapshot {
    let mut s = page("<html><body><p>no video here</p></body></html>");
    s.title = Some(title.to_string());
    s.network.push(NetworkEntry { url: OTHER.into(), method: Some("GET".into()), status: Some(200), mime: None, resource_type: None, frame_id: None });
    s.scripts = vec![r#"{"video":{"src":"https:\/\/cdn.news.example.gr\/play\/clip42"},"alt":"https://evil.example/v.mp4"}"#.into()];
    s
}

fn ptr_answer(ptr: &str) -> Value {
    json!({"assessment": "resolvable", "candidates": [{"from": "S1", "extract": {"kind": "ptr", "value": ptr}, "transforms": ["unescape"]}]})
}

fn llm_on() -> LlmCfg {
    LlmCfg { enabled: true, model: Some("test-model".into()), timeout: Duration::from_secs(5), max_calls_per_day: 15 }
}

fn llm_rig_with(mode: RepairMode, budget: Duration, mut fake: Fake, model: MockModel) -> Rig {
    fake.model = Some(model);
    let mut r = rig_with(mode, budget, fake);
    r.ctx.cfg.llm = llm_on();
    r
}

fn llm_rig(mode: RepairMode, model: MockModel) -> Rig {
    llm_rig_with(mode, Duration::from_secs(60), Fake::new(), model)
}

fn mock(r: &Rig) -> &MockModel {
    r.fake.model.as_ref().unwrap()
}

fn held_once(r: &Rig) {
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Another leased job of the rig's store.
fn next_job(r: &Rig, n: u32, url: &str) -> Job {
    // The same link for the same journalist is a duplicate while the first is active: another query each time.
    let url = format!("{url}?job={n}");
    r.repo.enqueue(&NewJob::new(&url, &format!("{n}_ANNA_TEST"), "ANNA"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap();
    r.repo.lease_job(OWNER, 600).unwrap().expect("leased")
}

async fn run_on(ctx: &RepairCtx, job: &Job, url: &str, snap: PageSnapshot) -> LadderResult {
    run(ctx, job, OWNER, url, Some(snap), &Prior::default(), &CancellationToken::new()).await
}

fn llm_recipes(r: &Rig) -> Vec<omni_core::recipe_score::RankedRecipe> {
    r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap().into_iter().filter(|x| x.origin == Origin::Llm).collect()
}

/// A recipe that reads the script's `src` and gives back exactly `CLIP`.
fn good_recipe() -> Value {
    json!({"dsl_version": 1, "steps": [
        {"op": "re", "re": r#""src":"(https:[^"]+)""#},
        {"op": "unescape", "from": 0},
        {"op": "media", "from": 1, "referer": "page"}
    ]})
}

/// A valid recipe that finds nothing on the page.
fn wrong_recipe() -> Value {
    json!({"dsl_version": 1, "steps": [
        {"op": "re", "re": r#""nothing":"([^"]+)""#},
        {"op": "media", "from": 0, "referer": "page"}
    ]})
}

// ---- acceptance (8): a valid pointer into a JSON script window ----

#[tokio::test]
async fn a_pointer_into_a_json_script_window_is_downloaded_held_and_named_llm() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src")]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    let LadderResult::Held { recipe_hash } = res else { panic!("expected Held, got {res:?}") };
    assert!(!recipe_hash.is_empty());
    let d = r.fake.downloads();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].0, CLIP, "the address is the one computed from the page item");
    assert_eq!(d[0].2, DeliveryMode::Hold { hold_hours: 24, held_max_bytes: 20 << 30 }, "repaired videos always wait for MCR");
    assert_eq!(mock(&r).asks(), 1);
    let job = r.repo.get_job(r.job.id).unwrap().unwrap();
    let method = job.extraction_method.clone().unwrap();
    assert!(method.starts_with("repair:llm:") && method.len() == "repair:llm:".len() + 8, "{method}");
    assert_eq!(job.status, JobStatus::Running, "the ladder never sets a status");

    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.rung.as_deref(), at.outcome.as_deref()), (Some("llm"), Some("held")));
    assert_eq!((at.llm_calls, at.tokens_in, at.tokens_out), (1, 1000, 100), "calls and tokens are on the attempt");
    assert_eq!(at.held_url.as_deref(), Some(CLIP));
    let list: Value = serde_json::from_str(at.candidates_json.as_deref().unwrap()).unwrap();
    assert!(list[0]["origin"].as_str().unwrap().starts_with("llm:"), "{list}");
    assert_eq!((at.recipe_id, at.recipe_hash.clone()), (None, None), "no recipe was offered, none is stored");
    assert!(llm_recipes(&r).is_empty());
    assert_eq!(r.repo.llm_usage_for(&today()).unwrap(), (1, 1000, 100), "the day's totals");
}

// ---- acceptance (1) (2) (3) at the ladder ----

#[tokio::test]
async fn an_address_the_model_writes_is_never_used_only_the_one_computed_from_the_page() {
    let evil = "https://evil.example/x.mp4";
    let answer = json!({
        "assessment": "resolvable",
        "reason": format!("it is at {evil}"),
        "url": evil,
        "candidates": [
            {"from": "N7", "extract": {"kind": "whole", "value": evil}, "transforms": []},
            {"from": "S1", "extract": {"kind": "ptr", "value": evil}, "transforms": []},
            {"from": "S1", "extract": {"kind": "ptr", "value": "/video/src"}, "transforms": ["unescape"]}
        ]
    });
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    let urls: Vec<String> = r.fake.downloads().into_iter().map(|d| d.0).collect();
    assert_eq!(urls, vec![CLIP.to_string()], "only the computed address; not a hallucinated id, a literal address, or a foreign host");
    assert_eq!(r.fake.probes(), 1, "nothing else reached the gate");
}

#[tokio::test]
async fn a_literal_address_in_a_whole_extract_is_ignored_the_page_items_own_address_is_used() {
    let evil = "https://evil.example/x.mp4";
    // Even a literal address on a host the page saw is not taken from the answer.
    let seen_host = "https://cdn.news.example.gr/play/written-by-the-model";
    for value in [evil, seen_host] {
        let answer = json!({
            "assessment": "resolvable", "reason": value, "url": value,
            "candidates": [{"from": "N1", "extract": {"kind": "whole", "value": value}, "transforms": []}]
        });
        let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
        held_once(&r);
        let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
        assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
        let urls: Vec<String> = r.fake.downloads().into_iter().map(|d| d.0).collect();
        assert_eq!(urls, vec![OTHER.to_string()], "the address is N1's own, not the text the model wrote ({value})");
    }
}

#[tokio::test]
async fn an_answer_of_only_foreign_or_unreadable_pointers_gives_nothing() {
    let evil = "https://evil.example/x.mp4";
    let answer = json!({
        "assessment": "resolvable", "reason": evil, "url": evil,
        "candidates": [
            {"from": "S1", "extract": {"kind": "ptr", "value": "/alt"}, "transforms": []},
            {"from": "S1", "extract": {"kind": "ptr", "value": evil}, "transforms": []},
            {"from": "N9", "extract": {"kind": "whole"}, "transforms": []}
        ]
    });
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert!(r.fake.downloads().is_empty());
    assert_eq!(r.fake.probes(), 0, "a foreign host, a literal address and an unknown id never reach the gate");
}

// ---- acceptance (4): rounds ----

#[tokio::test]
async fn rejected_candidates_are_not_offered_again_and_round_two_with_nothing_new_stops() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src"), ptr_answer("/video/src"), ptr_answer("/video/src")]));
    r.fake.probe.lock().unwrap().insert(CLIP.to_string(), Err("not a video".to_string()));
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(mock(&r).asks(), 2, "asked exactly twice: the second round brought nothing new");
    assert_eq!(r.fake.probes(), 1, "the rejected address is not probed again");
    assert!(r.fake.downloads().is_empty());
    // The second round was told what was tried: hosts, never addresses.
    let second = mock(&r).asked.lock().unwrap()[1].clone();
    let notes = &second[second.find("PAGE_DATA>>>").unwrap()..];
    assert!(notes.contains("EARLIER ATTEMPTS") && notes.contains("cdn.news.example.gr"), "{notes}");
    assert!(!notes.contains("clip42") && !notes.contains("http"), "no address in the notes: {notes}");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.llm_calls, at.tokens_in), (2, 2000));
}

#[tokio::test]
async fn addresses_the_normal_path_or_mcr_already_had_are_not_offered() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src")]));
    let prior = Prior { tried: vec![Tried { url: CLIP.into(), referer: Some(PAGE.into()) }], rejected_by_mcr: vec![], skips: vec![] };
    assert!(matches!(go(&r, Some(llm_page("a")), &prior).await, LadderResult::NothingVerified));
    assert_eq!((mock(&r).asks(), r.fake.probes()), (1, 0), "tried by the normal path: dropped before the gate");

    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src")]));
    let prior = Prior { tried: vec![], rejected_by_mcr: vec![CLIP.into()], skips: vec![] };
    assert!(matches!(go(&r, Some(llm_page("a")), &prior).await, LadderResult::NothingVerified));
    assert_eq!((mock(&r).asks(), r.fake.probes()), (1, 0), "rejected by MCR: never again");
}

/// P10.13: an address MCR called a wrong video on this site is dropped from what the
/// model pointed at, before the gate, whatever its query says.
#[tokio::test]
async fn an_address_mcr_called_wrong_is_dropped_from_the_models_pointer_before_the_gate() {
    let skip = omni_core::urlnorm::skip_pattern(&format!("{CLIP}?token=old")).unwrap();
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src")]));
    let prior = Prior { skips: vec![skip], ..Default::default() };
    assert!(matches!(go(&r, Some(llm_page("a")), &prior).await, LadderResult::NothingVerified));
    assert_eq!((mock(&r).asks(), r.fake.probes()), (1, 0), "skipped: the model was asked, the gate never saw it");
    assert!(r.fake.downloads().is_empty());

    // Control: another address's skip leaves the pointer alone.
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src")]));
    held_once(&r);
    let prior = Prior { skips: vec![omni_core::urlnorm::skip_pattern(CLIP99).unwrap()], ..Default::default() };
    assert!(matches!(go(&r, Some(llm_page("a")), &prior).await, LadderResult::Held { .. }));
    assert_eq!(r.fake.downloads()[0].0, CLIP);
}

// ---- acceptance (5): not resolvable ----

#[tokio::test]
async fn not_resolvable_stops_after_one_round_and_hold_mode_says_why() {
    for (reason, greek) in [
        ("drm", "το βίντεο φαίνεται κλειδωμένο (DRM)"),
        ("login", "η σελίδα ζητά σύνδεση"),
        ("needs_click", "χρειάζεται κλικ που δεν έγινε"),
        ("no_media_data", "η σελίδα δεν δίνει στοιχεία βίντεο"),
        ("other", "άλλος λόγος"),
    ] {
        let r = llm_rig(RepairMode::Hold, MockModel::new(vec![json!({"assessment": "not_resolvable", "reason": reason}), ptr_answer("/video/src")]));
        let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
        assert_eq!(mock(&r).asks(), 1, "stops at once");
        let LadderResult::Unresolvable { reason: why } = &res else { panic!("expected Unresolvable, got {res:?}") };
        assert_eq!(why.greek(), greek);
        let d = disposition(&res, RepairMode::Hold, Duration::from_secs(60));
        assert_eq!(d, Disposition::Annotate(greek.to_string()), "hold mode: the job's message gains the reason");
        assert_eq!(disposition(&res, RepairMode::Shadow, Duration::from_secs(60)), Disposition::Unchanged, "shadow never changes the outcome");
        assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().outcome.as_deref(), Some("not_resolvable"));
        assert!(r.fake.downloads().is_empty());
    }
    let m = annotate("Η σελίδα έχει βίντεο αλλά δεν κατέβηκε αυτόματα.", "άλλος λόγος");
    assert!(m.starts_with("Η σελίδα έχει βίντεο") && m.contains("άλλος λόγος"), "{m}");
    assert_eq!(annotate("", "άλλος λόγος"), "Η αυτόματη επισκευή: άλλος λόγος");
}

#[tokio::test]
async fn shadow_mode_with_a_not_resolvable_answer_changes_nothing() {
    let r = llm_rig(RepairMode::Shadow, MockModel::new(vec![json!({"assessment": "not_resolvable", "reason": "drm"})]));
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert_eq!(disposition(&res, RepairMode::Shadow, Duration::from_secs(60)), Disposition::Unchanged);
    assert!(events(&r).iter().all(|m| !m.contains("DRM")), "nothing written on the job in shadow mode");
}

// ---- acceptance (6): the daily cap ----

#[tokio::test]
async fn the_daily_cap_holds_across_jobs_and_a_reopened_store() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![]));
    let path = r._dir.0.join("omni.db");
    let mut ctx = RepairCtx { repo: r.repo.clone(), cfg: r.ctx.cfg.clone(), env: r.fake.clone() };
    for k in 0..16u32 {
        if k == 9 {
            // A restart in the middle of the day: same file, new handle.
            ctx = RepairCtx { repo: Repository::new(&path).unwrap(), cfg: r.ctx.cfg.clone(), env: r.fake.clone() };
        }
        // Another site and another page every time: neither the breaker nor the 24 h rule is in play.
        let url = format!("https://news.site{k}.example/story");
        let job = if k == 0 { r.job.clone() } else { next_job(&r, k + 1, &url) };
        let mut snap = PageSnapshot::new(&url);
        snap.html = Some("<html><body></body></html>".into());
        let res = run_on(&ctx, &job, &url, snap).await;
        assert!(matches!(res, LadderResult::NothingVerified), "job {k}: {res:?}");
        let want = (k + 1).min(15) as usize;
        assert_eq!(mock(&r).asks(), want, "job {k}: the 16th call of the day must be refused");
    }
    assert_eq!(r.repo.llm_usage_for(&today()).unwrap().0, 15);
}

// ---- acceptance (7): the breaker ----

#[tokio::test]
async fn three_misses_open_the_breaker_and_a_canary_pass_or_a_new_build_closes_it() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![]));
    // Three jobs on one site (different pages, so the 24 h bundle rule is not in play) find nothing.
    for k in 0..3u32 {
        let job = if k == 0 { r.job.clone() } else { next_job(&r, k + 1, PAGE) };
        assert!(matches!(run_on(&r.ctx, &job, PAGE, llm_page(&format!("story {k}"))).await, LadderResult::NothingVerified));
        assert_eq!(r.repo.llm_breaker_open(DOMAIN, Utc::now()).unwrap(), k == 2, "open after the third miss, not before");
    }
    assert_eq!(mock(&r).asks(), 3);

    // The fourth job: the recipe rung runs, the model is never asked.
    mock(&r).panics.store(true, Ordering::SeqCst);
    let job = next_job(&r, 4, PAGE);
    assert!(matches!(run_on(&r.ctx, &job, PAGE, llm_page("story 3")).await, LadderResult::NothingVerified));
    assert_eq!(mock(&r).asks(), 3);
    assert_eq!(r.repo.latest_attempt(job.id).unwrap().unwrap().llm_calls, 0);

    // A canary pass closes it (reset_site_failures is what the canary calls).
    r.repo.reset_site_failures(DOMAIN).unwrap();
    assert!(!r.repo.llm_breaker_open(DOMAIN, Utc::now()).unwrap());
    mock(&r).panics.store(false, Ordering::SeqCst);
    let job = next_job(&r, 5, PAGE);
    run_on(&r.ctx, &job, PAGE, llm_page("story 4")).await;
    assert_eq!(mock(&r).asks(), 4, "asked again after the canary");
    assert!(r.repo.llm_breaker_open(DOMAIN, Utc::now()).unwrap(), "another miss: three or more in 24 h opens it again");

    // So does a kept yt-dlp update.
    r.repo.close_all_llm_breakers().unwrap();
    let job = next_job(&r, 6, PAGE);
    run_on(&r.ctx, &job, PAGE, llm_page("story 5")).await;
    assert_eq!(mock(&r).asks(), 5, "asked again after close_all");
}

#[tokio::test]
async fn a_found_video_is_not_a_miss() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_answer("/video/src"), ptr_answer("/video/src"), ptr_answer("/video/src")]));
    for k in 0..3u32 {
        held_once(&r);
        let job = if k == 0 { r.job.clone() } else { next_job(&r, k + 1, PAGE) };
        assert!(matches!(run_on(&r.ctx, &job, PAGE, llm_page(&format!("story {k}"))).await, LadderResult::Held { .. }));
    }
    assert!(!r.repo.llm_breaker_open(DOMAIN, Utc::now()).unwrap());
    assert_eq!(r.repo.llm_misses_since(DOMAIN, Utc::now() - chrono::Duration::hours(24)).unwrap(), 0);
}

#[tokio::test]
async fn the_same_page_bundle_is_not_sent_twice_in_24_hours() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![]));
    assert!(matches!(go(&r, Some(llm_page("same page")), &Prior::default()).await, LadderResult::NothingVerified));
    assert_eq!(mock(&r).asks(), 1);
    let job = next_job(&r, 2, PAGE);
    assert!(matches!(run_on(&r.ctx, &job, PAGE, llm_page("same page")).await, LadderResult::NothingVerified));
    assert_eq!(mock(&r).asks(), 1, "the same bundle within 24 h: no call");
    assert_eq!(r.repo.latest_attempt(job.id).unwrap().unwrap().llm_calls, 0);
    // Another page is another bundle.
    let job = next_job(&r, 3, PAGE);
    run_on(&r.ctx, &job, PAGE, llm_page("another page")).await;
    assert_eq!(mock(&r).asks(), 2);
}

// ---- off by default ----

#[tokio::test]
async fn with_the_rung_off_or_no_model_named_the_model_is_never_asked() {
    for cfg in [
        LlmCfg { enabled: false, ..llm_on() },
        LlmCfg { model: None, ..llm_on() },
        LlmCfg { model: Some("  ".into()), ..llm_on() },
        LlmCfg::default(),
    ] {
        let model = MockModel::new(vec![ptr_answer("/video/src")]);
        model.panics.store(true, Ordering::SeqCst);
        let mut r = llm_rig(RepairMode::Hold, model);
        r.ctx.cfg.llm = cfg;
        let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
        assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
        assert_eq!(mock(&r).asks(), 0);
        assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().llm_calls, 0);
    }
    // Enabled and named, but the environment has no model.
    let mut r = rig(RepairMode::Hold);
    r.ctx.cfg.llm = llm_on();
    assert!(matches!(go(&r, Some(llm_page("Invented story")), &Prior::default()).await, LadderResult::NothingVerified));
    assert!(!LlmCfg::default().active() && llm_on().active());
}

#[tokio::test]
async fn a_recipe_that_finds_the_video_means_the_model_is_not_asked() {
    let model = MockModel::new(vec![]);
    model.panics.store(true, Ordering::SeqCst);
    let r = llm_rig(RepairMode::Shadow, model);
    let res = go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }), "{res:?}");
}

// ---- the real client and the redaction ----

/// A one-answer HTTP server on a local port that keeps every request body.
async fn local_model_server() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let keep = bodies.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let keep = keep.clone();
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buf[..h]).to_ascii_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        keep.lock().unwrap().push(String::from_utf8_lossy(&buf[h + 4..h + 4 + len]).to_string());
                        break;
                    }
                }
                let body = r#"{"choices":[{"message":{"content":"{\"assessment\":\"not_resolvable\",\"reason\":\"other\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#;
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (addr, bodies)
}

#[tokio::test]
async fn contact_details_are_redacted_for_an_online_endpoint_and_not_for_a_local_one() {
    let (addr, bodies) = local_model_server().await;
    let user = "script: contact anna.test@example.org or call +30 2810 123456 about https://cdn.news.example.gr/play/clip42";
    let client = || LlmClient::with_timeout(&format!("http://{addr}/v1"), "test-model", Duration::from_secs(10)).with_reasoning_effort(Some("medium".into()));

    // A local endpoint is not redacted (nothing leaves the building).
    let local = LlmModel::new(client(), 100);
    assert!(!local.redact, "127.0.0.1 is local");
    let (answer, usage) = local.ask("system", user, &json!({"type": "object"})).await.unwrap();
    assert_eq!(answer["assessment"], "not_resolvable");
    assert_eq!(usage, Usage { prompt_tokens: 7, completion_tokens: 3 });
    // An online one is, and the link survives.
    LlmModel::new(client(), 100).with_redaction(true).ask("system", user, &json!({"type": "object"})).await.unwrap();
    assert!(LlmModel::new(LlmClient::new("https://api.example.com/v1", "m"), 100).redact, "an online endpoint redacts by default");

    let seen = bodies.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].contains("anna.test@example.org") && seen[0].contains("2810 123456"), "local: untouched");
    assert!(!seen[1].contains("anna.test@example.org") && !seen[1].contains("2810 123456"), "online: contact details never leave: {}", seen[1]);
    assert!(seen[1].contains("[email]") && seen[1].contains("[phone]"));
    assert!(seen[1].contains("https://cdn.news.example.gr/play/clip42"), "the link is kept");
    assert!(seen[1].contains(r#""reasoning_effort":"medium""#), "the configured effort is sent");
}

// ---- the click ----

fn click_answer() -> Value {
    json!({"assessment": "resolvable", "candidates": [], "action": {"click": "button.play"}})
}

#[tokio::test]
async fn a_click_uses_the_one_live_load_and_the_next_round_sees_the_clicked_page() {
    let fake = Fake::new();
    let mut live = llm_page("Clicked page");
    live.scripts = vec![r#"{"video":{"src":"https:\/\/cdn.news.example.gr\/play\/clip99"}}"#.into()];
    set_live(&fake, live, LiveRun::Complete, &["clicked"]);
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(120), fake, MockModel::new(vec![click_answer(), ptr_answer("/video/src")]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*r.fake.live_seen.lock().unwrap(), vec![vec![LiveAction::Click("button.play".into())]]);
    assert_eq!(mock(&r).asks(), 2);
    let asked = mock(&r).asked.lock().unwrap().clone();
    assert!(!asked[0].contains("clip99") && asked[1].contains("clip99"), "round two is built from the page after the click");
    assert!(asked[1].contains("your click was done"));
    let d = r.fake.downloads();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].0, CLIP99, "the address comes from the live page's own item");
}

#[tokio::test]
async fn a_click_is_not_done_when_the_recipe_rung_used_the_live_load() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[]), LiveRun::Complete, &["clicked"]);
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(120), fake, MockModel::new(vec![click_answer(), click_answer()]));
    add_live_recipe(&r, "site", "#load", "zzz[.]m3u8");
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 1, "the recipe rung's live load was the only one");
    assert_eq!(mock(&r).asks(), 1, "nothing to click, nothing new: stop");
}

#[tokio::test]
async fn a_click_is_not_done_without_the_budget_for_it_and_the_gate() {
    let mut fake = Fake::new();
    fake.live_panics = true;
    // 45 s: enough for the model round (35 s), not for a live load plus the gate's reserve (50 s).
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(45), fake, MockModel::new(vec![click_answer()]));
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 0);
    assert_eq!(mock(&r).asks(), 1);
}

#[tokio::test]
async fn a_click_the_whitelist_refuses_is_not_done() {
    let mut fake = Fake::new();
    fake.live_panics = true;
    let bad = json!({"assessment": "resolvable", "candidates": [], "action": {"click": "div:has(a)"}});
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(120), fake, MockModel::new(vec![bad]));
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 0);
}

// ---- time and cancel ----

#[tokio::test]
async fn too_little_budget_means_no_model_call() {
    let model = MockModel::new(vec![ptr_answer("/video/src")]);
    model.panics.store(true, Ordering::SeqCst);
    // The gate's 20 s and 15 s for a round: under 35 s the rung does not start.
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(34), Fake::new(), model);
    assert!(matches!(go(&r, Some(llm_page("Invented story")), &Prior::default()).await, LadderResult::NothingVerified));
}

#[tokio::test]
async fn a_hanging_model_call_ends_at_its_timeout_and_the_ladder_stays_inside_the_budget() {
    let model = MockModel { hang: true, ..MockModel::new(vec![]) };
    let mut r = llm_rig_with(RepairMode::Hold, Duration::from_secs(60), Fake::new(), model);
    r.ctx.cfg.llm.timeout = Duration::from_secs(1);
    let t = Instant::now();
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    let took = t.elapsed();
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert!(took >= Duration::from_millis(900) && took < Duration::from_secs(10), "{took:?}");
    assert!(took < Duration::from_secs(60) + GRACE);
    assert_eq!(mock(&r).asks(), 1);
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.llm_calls, at.tokens_in, at.tokens_out), (1, 0, 0), "the call is counted, no tokens were reported");
}

#[tokio::test]
async fn the_model_call_never_outlasts_the_budget_less_the_gate_reserve() {
    // 36 s of budget, a configured timeout of an hour: the call may take 16 s at most.
    let model = MockModel { hang: true, ..MockModel::new(vec![]) };
    let mut r = llm_rig_with(RepairMode::Hold, Duration::from_secs(36), Fake::new(), model);
    r.ctx.cfg.llm.timeout = Duration::from_secs(3600);
    let t = Instant::now();
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    let took = t.elapsed();
    assert!(matches!(res, LadderResult::NothingVerified | LadderResult::Exhausted), "{res:?}");
    assert!(took <= Duration::from_secs(36) + GRACE, "{took:?}");
    assert!(took <= Duration::from_secs(17), "the call ended at budget - gate reserve: {took:?}");
}

#[tokio::test]
async fn cancelling_during_a_model_call_is_lost_and_writes_nothing() {
    let model = MockModel { hang: true, ..MockModel::new(vec![]) };
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(60), Fake::new(), model);
    let token = CancellationToken::new();
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel();
    });
    let t = Instant::now();
    let res = run(&r.ctx, &r.job, OWNER, PAGE, Some(llm_page("Invented story")), &Prior::default(), &token).await;
    assert!(matches!(res, LadderResult::Lost), "{res:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(events(&r).iter().all(|m| !m.contains("επισκευή")), "nothing written on the job: {:?}", events(&r));
    assert!(r.fake.downloads().is_empty());
    assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().outcome.as_deref(), Some("lost"));
    assert_eq!(r.repo.get_job(r.job.id).unwrap().unwrap().extraction_method, None);
}

#[tokio::test]
async fn a_failing_model_call_is_a_miss_and_never_an_error_for_the_job() {
    let model = MockModel::new(vec![]);
    model.answers.lock().unwrap().push_back(Err("the provider is down".into()));
    let r = llm_rig(RepairMode::Hold, model);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().llm_calls, 1);
    assert_eq!(mock(&r).asks(), 1, "no second round after a failed call");
}

// ---- acceptance (9): the recipe ----

#[tokio::test]
async fn a_recipe_that_reproduces_the_address_is_stored_for_the_site_and_linked_on_the_attempt() {
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": good_recipe().to_string()});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    let rec = Recipe::from_json(&good_recipe().to_string()).unwrap();
    let scope = recipe::scope_hash("site", DOMAIN, &rec);
    let LadderResult::Held { recipe_hash } = res else { panic!("expected Held, got {res:?}") };
    assert_eq!(recipe_hash, scope);
    let stored = llm_recipes(&r);
    assert_eq!(stored.len(), 1, "one llm recipe in the store");
    assert_eq!((stored[0].scope_kind, stored[0].scope_key.as_str(), stored[0].hash.as_str()), (ScopeKind::Site, DOMAIN, scope.as_str()));
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.recipe_id, at.recipe_hash.as_deref()), (Some(stored[0].id), Some(scope.as_str())), "linked on the attempt");
    assert_eq!(r.repo.recipe_origin(stored[0].id).unwrap().as_deref(), Some("llm"));
    let method = r.repo.get_job(r.job.id).unwrap().unwrap().extraction_method.unwrap();
    assert_eq!(method, format!("repair:llm:{}", &scope[..8]), "the method names the reproduced recipe");
}

/// P10.13: the model's recipe merges into a player recipe with the same steps. The attempt must
/// name that player recipe by its own hash, so MCR's «Λάθος βίντεο» suspends and bars it.
#[tokio::test]
async fn a_model_recipe_merged_into_a_player_recipe_is_judged_as_the_player_recipe() {
    use omni_core::recipe_score::NewRecipe;
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": good_recipe().to_string()});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    held_once(&r);
    let rec = Recipe::from_json(&good_recipe().to_string()).unwrap();
    let steps = recipe::steps_hash(&rec);
    let player = NewRecipe {
        scope_kind: ScopeKind::Player,
        scope_key: "hlsjs".into(),
        family: Some("hlsjs".into()),
        recipe_json: rec.canonical_json(),
        hash: "player-hash-for-test".into(),
        steps_hash: steps.clone(),
        dsl_version: rec.dsl_version,
        origin: Origin::Admin,
    };
    let player_id = match r.repo.insert_recipe(&player, Utc::now()).unwrap() {
        InsertOutcome::Inserted(id) => id,
        other => panic!("{other:?}"),
    };
    // The player recipe is suspended on this site (failing), so the recipe rung skips it and the
    // model rung finds the video; the model's recipe then merges into it. No bar is written yet.
    for _ in 0..3 {
        r.repo.recipe_event(player_id, DOMAIN, Event::NoCandidate, Utc::now()).unwrap();
    }
    assert!(!r.repo.recipe_steps_barred(&steps, DOMAIN).unwrap());
    let mut snap = llm_page("Invented story");
    snap.html = Some(r#"<html><body><script src="/static/hls.js"></script><p>no video here</p></body></html>"#.into());
    let res = go(&r, Some(snap), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.recipe_id, at.recipe_hash.as_deref()), (Some(player_id), Some("player-hash-for-test")), "the player recipe, with its own hash");

    // Park it and let MCR call it the wrong video.
    assert!(r.repo.hold_output(r.job.id, OWNER, "temp/held/x/clip.mxf", 3, &"ab".repeat(32), Some(9.0), Utc::now() + chrono::Duration::hours(24)).unwrap());
    assert!(crate::finish_held(&r.repo, r.job.id, OWNER).unwrap());
    assert!(r.repo.reject_held(r.job.id, "wrong_video", "no", "desk").unwrap());
    assert!(r.repo.recipe_steps_barred(&steps, DOMAIN).unwrap(), "the steps are barred on the domain");
    let offered = r.repo.recipes_for_site(DOMAIN, Some("hlsjs"), Utc::now()).unwrap();
    assert!(offered.iter().all(|x| x.id != player_id), "the player recipe is suspended there");
    let rejections: i64 = {
        // The Rejection landed on the player recipe's row (a missing event would leave it at 0).
        let row = r.repo.recipe_event(player_id, DOMAIN, Event::NoCandidate, Utc::now()).unwrap();
        row.rejections
    };
    assert_eq!(rejections, 1);
}

#[tokio::test]
async fn a_recipe_that_does_not_reproduce_the_address_is_discarded_and_the_video_is_still_held() {
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": wrong_recipe().to_string()});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert!(llm_recipes(&r).is_empty(), "a recipe that does not reproduce the candidate is not stored");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.recipe_id, at.recipe_hash, at.outcome.as_deref()), (None, None, Some("held")));
    assert_eq!(r.fake.downloads()[0].0, CLIP);
}

#[tokio::test]
async fn an_invalid_recipe_is_discarded_and_the_video_is_still_held() {
    let bad = json!({"dsl_version": 1, "steps": [{"op": "shell", "cmd": "calc"}]});
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": bad.to_string()});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    held_once(&r);
    assert!(matches!(go(&r, Some(llm_page("Invented story")), &Prior::default()).await, LadderResult::Held { .. }));
    assert!(llm_recipes(&r).is_empty());
}

#[tokio::test]
async fn no_recipe_is_stored_when_nothing_is_held() {
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": good_recipe().to_string()});
    // The download fails: no video is held.
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer.clone()]));
    r.fake.script.lock().unwrap().push_back(Err(ErrorCode::Http403));
    assert!(matches!(go(&r, Some(llm_page("Invented story")), &Prior::default()).await, LadderResult::NothingVerified));
    assert!(llm_recipes(&r).is_empty(), "no held video, no recipe");
    // The gate rejects it.
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer.clone()]));
    r.fake.probe.lock().unwrap().insert(CLIP.to_string(), Err("not a video".to_string()));
    go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(llm_recipes(&r).is_empty());
    // Shadow mode downloads nothing and stores nothing.
    let r = llm_rig(RepairMode::Shadow, MockModel::new(vec![answer]));
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    let LadderResult::ShadowFound { verified } = res else { panic!("expected a shadow find, got {res:?}") };
    assert!(verified[0].candidate.origin.starts_with("llm:"));
    assert!(llm_recipes(&r).is_empty() && r.fake.downloads().is_empty());
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.mode.as_str(), at.rung.as_deref(), at.outcome.as_deref()), ("shadow", Some("llm"), Some("shadow_found")));
    assert_eq!(r.repo.get_job(r.job.id).unwrap().unwrap().extraction_method, None, "shadow changes nothing on the job");
}

#[tokio::test]
async fn a_recipe_whose_steps_mcr_rejected_on_this_site_drops_the_answers_candidates() {
    use omni_core::recipe_score::{Event, InsertOutcome, NewRecipe};
    let answer = json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": good_recipe().to_string()});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    // The same steps were stored for this site and MCR rejected their video: a bar.
    let rec = Recipe::from_json(&good_recipe().to_string()).unwrap();
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
    let (InsertOutcome::Inserted(id) | InsertOutcome::Merged(id)) = r.repo.insert_recipe(&new, Utc::now()).unwrap() else { panic!("not stored") };
    r.repo.recipe_event(id, DOMAIN, Event::Rejection, Utc::now()).unwrap();
    assert!(r.repo.recipe_steps_barred(&recipe::steps_hash(&rec), DOMAIN).unwrap());

    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!((mock(&r).asks(), r.fake.probes()), (1, 0), "asked, but the barred recipe's candidates never reach the gate");
}

#[tokio::test]
async fn the_bundle_the_model_sees_is_fenced_and_holds_the_page() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![]));
    go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    let sent = mock(&r).asked.lock().unwrap()[0].clone();
    assert!(sent.starts_with("UNTRUSTED PAGE DATA"), "{}", &sent[..60]);
    assert!(sent.contains("<<<PAGE_DATA") && sent.contains("PAGE_DATA>>>") && sent.contains("[S1]") && sent.contains("clip42"));
    assert!(sent.contains("EARLIER ATTEMPTS"), "the recipe rung's miss is told to the model");
}

// ---- P10.12 review fixes ----

fn literal_template_recipe() -> Value {
    json!({"dsl_version": 1, "steps": [
        {"op": "template", "t": CLIP},
        {"op": "media", "from": 0, "referer": "page"}
    ]})
}

fn ptr_with_recipe(recipe: Value) -> Value {
    json!({"assessment": "resolvable", "candidates": [ptr_answer("/video/src")["candidates"][0].clone()], "recipe": recipe.to_string()})
}

#[tokio::test]
async fn a_recipe_with_an_address_of_its_own_is_discarded_and_the_candidate_is_still_held() {
    // The template has no slot: its output is the same whatever the page says.
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_with_recipe(literal_template_recipe())]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.downloads()[0].0, CLIP, "the candidate itself is used");
    assert!(llm_recipes(&r).is_empty(), "a recipe that does not read the page must never be stored");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.recipe_id, at.recipe_hash), (None, None));
}

#[tokio::test]
async fn a_recipe_that_still_gives_the_address_without_the_source_item_is_not_kept() {
    // The same address sits in a second script: blanking S1 does not change the recipe's output.
    let mut page = llm_page("Invented story");
    page.scripts.push(page.scripts[0].clone());
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_with_recipe(good_recipe())]));
    held_once(&r);
    let res = go(&r, Some(page), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert!(llm_recipes(&r).is_empty(), "the rule: a kept recipe loses its address when the source item is taken out");
}

#[tokio::test]
async fn a_recipe_that_reads_the_address_from_the_source_item_is_kept() {
    // N1 is the page's own network entry; the recipe finds it by pattern, and without N1 finds nothing.
    let recipe = json!({"dsl_version": 1, "steps": [
        {"op": "net", "url_re": "play/other\\d+"},
        {"op": "media", "from": 0, "referer": "page"}
    ]});
    let answer = json!({"assessment": "resolvable", "candidates": [{"from": "N1", "extract": {"kind": "whole"}, "transforms": []}], "recipe": recipe.to_string()});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.downloads()[0].0, OTHER);
    assert_eq!(llm_recipes(&r).len(), 1, "page-dependent and reproducing: stored");
}

// ---- needs a click: both answer forms ----

fn live_clip99_page() -> PageSnapshot {
    let mut live = llm_page("Clicked page");
    live.scripts = vec![r#"{"video":{"src":"https:\/\/cdn.news.example.gr\/play\/clip99"}}"#.into()];
    live
}

#[tokio::test]
async fn a_not_resolvable_needs_click_answer_with_a_click_takes_the_click_round() {
    let fake = Fake::new();
    set_live(&fake, live_clip99_page(), LiveRun::Complete, &["clicked"]);
    let form_b = json!({"assessment": "not_resolvable", "reason": "needs_click", "action": {"click": "button.play"}});
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(120), fake, MockModel::new(vec![form_b, ptr_answer("/video/src")]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(*r.fake.live_seen.lock().unwrap(), vec![vec![LiveAction::Click("button.play".into())]]);
    assert_eq!(mock(&r).asks(), 2);
    assert!(mock(&r).asked.lock().unwrap()[1].contains("clip99"), "round two sees the page after the click");
    assert_eq!(r.fake.downloads()[0].0, CLIP99);
}

#[tokio::test]
async fn the_prompt_asks_for_a_click_as_a_resolvable_answer() {
    let p = omni_browser::repair_llm::REPAIR_PROMPT_V1;
    assert!(p.contains("\"resolvable\" with an empty candidates list") && p.contains("action.click"));
    assert!(!p.contains("not_resolvable\" with the reason (drm, login, needs_click"), "the old form is gone");
    assert_eq!(omni_browser::repair_llm::answer_schema()["properties"]["recipe"]["type"], "string");
}

#[tokio::test]
async fn needs_click_without_a_click_or_without_the_live_load_ends_with_the_greek_reason() {
    let mut fake = Fake::new();
    fake.live_panics = true;
    let no_click = json!({"assessment": "not_resolvable", "reason": "needs_click"});
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(120), fake, MockModel::new(vec![no_click]));
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    let LadderResult::Unresolvable { reason } = res else { panic!("{res:?}") };
    assert_eq!(reason.greek(), "χρειάζεται κλικ που δεν έγινε");
    assert_eq!(mock(&r).asks(), 1);

    // The recipe rung used the one live load: the click is not taken.
    let fake = Fake::new();
    set_live(&fake, live_page(&[]), LiveRun::Complete, &["clicked"]);
    let with_click = json!({"assessment": "not_resolvable", "reason": "needs_click", "action": {"click": "button.play"}});
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(120), fake, MockModel::new(vec![with_click]));
    add_live_recipe(&r, "site", "#load", "zzz[.]m3u8");
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Unresolvable { .. }), "{res:?}");
    assert_eq!((r.fake.live_calls.load(Ordering::SeqCst), mock(&r).asks()), (1, 1));
}

#[tokio::test]
async fn a_failed_live_load_of_the_recipe_rung_still_spends_the_one_live_load() {
    // The browser did not start (live_snapshot returns None): the attempt counts.
    let fake = Fake::new();
    let r = llm_rig_with(RepairMode::Hold, Duration::from_secs(120), fake, MockModel::new(vec![click_answer()]));
    add_live_recipe(&r, "site", "#load", "zzz[.]m3u8");
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 1, "no second browser for the model's click");
    assert_eq!(mock(&r).asks(), 1);
}

#[tokio::test]
async fn a_template_assembled_address_recipe_is_discarded_and_the_candidate_is_still_held() {
    // Pieces read from the page, joined by a template around a hard-coded host: not page-derived.
    let bypass = json!({"dsl_version": 1, "steps": [
        {"op": "re", "re": r#""video":\{"src":"(h)"#},
        {"op": "re", "re": "https:(//)"},
        {"op": "template", "t": "{0}ttps:{1}cdn.news.example.gr/play/clip42"},
        {"op": "media", "from": 2, "referer": "page"}
    ]});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_with_recipe(bypass)]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.downloads()[0].0, CLIP);
    assert!(llm_recipes(&r).is_empty(), "no template recipe is ever stored for a model");
}

#[tokio::test]
async fn a_template_free_recipe_reading_the_source_item_is_kept() {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_with_recipe(good_recipe())]));
    held_once(&r);
    assert!(matches!(go(&r, Some(llm_page("Invented story")), &Prior::default()).await, LadderResult::Held { .. }));
    assert_eq!(llm_recipes(&r).len(), 1);
}

#[tokio::test]
async fn a_net_recipe_on_the_items_own_host_is_judged_by_the_content_not_the_host() {
    // The only entry on the CDN host is the source item. Blanked, it keeps its host but not its path,
    // so the recipe no longer gives the address and is kept; a recipe that still would is not.
    let by_host = json!({"dsl_version": 1, "steps": [
        {"op": "net", "url_re": "cdn[.]news[.]example[.]gr"},
        {"op": "media", "from": 0, "referer": "page"}
    ]});
    let answer = json!({"assessment": "resolvable", "candidates": [{"from": "N1", "extract": {"kind": "whole"}, "transforms": []}], "recipe": by_host.to_string()});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    held_once(&r);
    let res = go(&r, Some(llm_page("Invented story")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(llm_recipes(&r).len(), 1, "it reads the entry: without the entry's path it gives something else");
}

// ---- the recipe must follow the item, not carry the address ----

const VID: &str = "https://cdn.news.example.gr/play/clip42.vid";
const NEWS: &str = "https://cdn.news.example.gr/v/breaking-news.vid";

/// A page whose script holds a plain address (an extension no shipped recipe looks for).
fn plain_page(url: &str) -> PageSnapshot {
    let mut s = page("<html><body><p>no video here</p></body></html>");
    s.title = Some("Invented story".into());
    s.scripts = vec![format!(r#"{{"video":{{"src":"{url}"}}}}"#)];
    s
}

async fn held_with_recipe(page: PageSnapshot, recipe: Value) -> (Rig, LadderResult) {
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![ptr_with_recipe(recipe)]));
    held_once(&r);
    let res = go(&r, Some(page), &Prior::default()).await;
    (r, res)
}

fn re_recipe(re: &str) -> Value {
    json!({"dsl_version": 1, "steps": [{"op": "re", "re": re}, {"op": "media", "from": 0, "referer": "page"}]})
}

#[tokio::test]
async fn a_recipe_with_this_one_video_written_into_it_is_discarded_and_the_candidate_is_still_held() {
    // The same recipe as the generic one below, but with clip42 in the pattern.
    let (r, res) = held_with_recipe(plain_page(VID), re_recipe(r#"(https://cdn\.news\.example\.gr/play/clip42\.vid)"#)).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.downloads()[0].0, VID);
    assert!(llm_recipes(&r).is_empty(), "a recipe that only finds this clip must not be stored");

    // A hard-coded Net pattern on the page's own network entry (N1, other77).
    let net = json!({"dsl_version": 1, "steps": [{"op": "net", "url_re": "play/other77"}, {"op": "media", "from": 0, "referer": "page"}]});
    let answer = json!({"assessment": "resolvable", "candidates": [{"from": "N1", "extract": {"kind": "whole"}, "transforms": []}], "recipe": net.to_string()});
    let r = llm_rig(RepairMode::Hold, MockModel::new(vec![answer]));
    held_once(&r);
    assert!(matches!(go(&r, Some(llm_page("Invented story")), &Prior::default()).await, LadderResult::Held { .. }));
    assert_eq!(r.fake.downloads()[0].0, OTHER);
    assert!(llm_recipes(&r).is_empty());
}

#[tokio::test]
async fn generic_recipes_that_follow_the_item_are_kept() {
    // A generic pattern, and a digit pattern that keeps its shape when the digits change.
    for re in [r#""src":"(https://[^"]+)""#, r#"(https://[^"]+/play/clip\d+\.vid)"#] {
        let (r, res) = held_with_recipe(plain_page(VID), re_recipe(re)).await;
        assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
        assert_eq!(llm_recipes(&r).len(), 1, "{re}");
    }
}

#[tokio::test]
async fn a_letters_only_stem_is_checked_by_rot13_and_a_hard_coded_name_is_discarded() {
    let (r, res) = held_with_recipe(plain_page(NEWS), re_recipe(r#""src":"(https://[^"]+)""#)).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(llm_recipes(&r).len(), 1, "a generic recipe follows the item");
    let (r, res) = held_with_recipe(plain_page(NEWS), re_recipe(r#"(https://cdn\.news\.example\.gr/v/breaking-news\.vid)"#)).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert!(llm_recipes(&r).is_empty(), "breaking-news is written into the pattern");
}
