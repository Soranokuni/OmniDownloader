//! Tests of the repair ladder (plan P10.11a). Everything outside the ladder is
//! scripted; the last test runs the real pipeline (skipped without `bin/` tools).
//! All pages, names and addresses are invented.

use super::*;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use omni_broadcast::repair_gate::{ProbeInfo, ProbeKind};
use omni_browser::sniffer::LiveRun;
use omni_browser::NetworkEntry;
use omni_core::models::{JobStatus, NewJob};
use omni_core::net::EgressError;

const OWNER: &str = "w1";
const PAGE: &str = "https://news.example.gr/story/1";
const DOMAIN: &str = "example.gr";
const A: &str = "https://cdn.news.example.gr/v/a.mp4";
const B: &str = "https://cdn.news.example.gr/v/b.mp4";
const C: &str = "https://cdn.news.example.gr/v/c.mp4";
const D: &str = "https://cdn.news.example.gr/v/d.mp4";

/// A scratch directory that removes itself.
struct Scratch(PathBuf);
static SCRATCH_SEQ: AtomicUsize = AtomicUsize::new(0);

impl Scratch {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("omni-repair-test-{}-{nanos}-{}", std::process::id(), SCRATCH_SEQ.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct NoFetch;
#[async_trait]
impl Fetcher for NoFetch {
    async fn get(&self, _url: &str, _deadline: Instant) -> Result<String> {
        Err(anyhow::anyhow!("no network in tests"))
    }
}

/// Everything outside the ladder, scripted.
struct Fake {
    diagnose_snap: Mutex<Option<PageSnapshot>>,
    diagnose_calls: AtomicUsize,
    diagnose_hangs: bool,
    probe: Mutex<HashMap<String, std::result::Result<ProbeInfo, String>>>,
    probe_delay: Duration,
    probe_calls: AtomicUsize,
    resolves_to: Mutex<HashMap<String, IpAddr>>,
    host_public: AtomicBool,
    downloads: Mutex<Vec<(String, Option<String>, DeliveryMode)>>,
    script: Mutex<VecDeque<std::result::Result<RunOutcome, ErrorCode>>>,
    fetch: NoFetch,
    /// ffmpeg for the card evidence; `None` = the ladder gets no card tools.
    card_ffmpeg: Option<PathBuf>,
    card_cap: Option<Duration>,
    pics: Pics,
    // The live page load (P10.11c): what it returns, and what it was asked.
    live_result: Mutex<Option<(PageSnapshot, LiveRun, Vec<String>)>>,
    live_calls: AtomicUsize,
    live_seen: Mutex<Vec<Vec<LiveAction>>>,
    live_secs_seen: Mutex<Vec<u64>>,
    live_hangs: bool,
    live_panics: bool,
    live_limits: Option<(u64, Duration)>,
    /// The scripted model of the LLM rung (P10.12); `None`: the ladder has no model.
    model: Option<MockModel>,
}

/// The model behind [`RepairModel`]: scripted answers, every user text kept.
struct MockModel {
    answers: Mutex<VecDeque<std::result::Result<serde_json::Value, String>>>,
    asked: Mutex<Vec<String>>,
    usage: Usage,
    hang: bool,
    /// Any call fails the test.
    panics: AtomicBool,
}

impl MockModel {
    fn new(answers: Vec<serde_json::Value>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().map(Ok).collect()),
            asked: Mutex::new(Vec::new()),
            usage: Usage { prompt_tokens: 1000, completion_tokens: 100 },
            hang: false,
            panics: AtomicBool::new(false),
        }
    }
    fn asks(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

#[async_trait]
impl RepairModel for MockModel {
    async fn ask(&self, _system: &str, user: &str, _schema: &serde_json::Value) -> Result<(serde_json::Value, Usage)> {
        assert!(!self.panics.load(Ordering::SeqCst), "the ladder asked the model");
        self.asked.lock().unwrap().push(user.to_string());
        if self.hang {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        match self.answers.lock().unwrap().pop_front() {
            Some(Ok(v)) => Ok((v, self.usage)),
            Some(Err(e)) => Err(anyhow::anyhow!(e)),
            None => Ok((serde_json::json!({"assessment": "resolvable", "candidates": []}), self.usage)),
        }
    }
}

/// The article pictures of the card step: one body or a failure, counted; `hook` runs at each request.
#[derive(Default)]
struct Pics {
    calls: AtomicUsize,
    hang: AtomicBool,
    body: Mutex<Option<Vec<u8>>>,
    hook: Mutex<Option<Box<dyn Fn() + Send>>>,
}
#[async_trait]
impl ImageSource for Pics {
    async fn get_image(&self, _url: &str, _deadline: Instant) -> Result<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(h) = self.hook.lock().unwrap().as_ref() {
            h();
        }
        if self.hang.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        self.body.lock().unwrap().clone().ok_or_else(|| anyhow::anyhow!("scripted picture failure"))
    }
}

fn info(secs: f64) -> ProbeInfo {
    ProbeInfo { kind: ProbeKind::Direct, video_streams: 1, height: 720, duration_secs: Some(secs), live: false }
}

impl Fake {
    fn new() -> Self {
        Self {
            diagnose_snap: Mutex::new(None),
            diagnose_calls: AtomicUsize::new(0),
            diagnose_hangs: false,
            probe: Mutex::new(HashMap::new()),
            probe_delay: Duration::ZERO,
            probe_calls: AtomicUsize::new(0),
            resolves_to: Mutex::new(HashMap::new()),
            host_public: AtomicBool::new(true),
            downloads: Mutex::new(Vec::new()),
            script: Mutex::new(VecDeque::new()),
            fetch: NoFetch,
            card_ffmpeg: None,
            card_cap: None,
            pics: Pics::default(),
            live_result: Mutex::new(None),
            live_calls: AtomicUsize::new(0),
            live_seen: Mutex::new(Vec::new()),
            live_secs_seen: Mutex::new(Vec::new()),
            live_hangs: false,
            live_panics: false,
            live_limits: None,
            model: None,
        }
    }
    fn probes(&self) -> usize {
        self.probe_calls.load(Ordering::SeqCst)
    }
    fn downloads(&self) -> Vec<(String, Option<String>, DeliveryMode)> {
        self.downloads.lock().unwrap().clone()
    }
}

#[async_trait]
impl Prober for Fake {
    async fn probe(&self, cand: &Candidate) -> std::result::Result<ProbeInfo, String> {
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        if !self.probe_delay.is_zero() {
            tokio::time::sleep(self.probe_delay).await;
        }
        self.probe.lock().unwrap().get(&cand.url).cloned().unwrap_or_else(|| Ok(info(120.0)))
    }
}

#[async_trait]
impl Resolver for Fake {
    async fn resolve(&self, host: &str, _port: u16) -> std::result::Result<Vec<IpAddr>, EgressError> {
        let ip = self.resolves_to.lock().unwrap().get(host).copied().unwrap_or_else(|| "93.184.216.34".parse().unwrap());
        Ok(vec![ip])
    }
}

#[async_trait]
impl Env for Fake {
    async fn diagnose(&self, _url: &str, _secs: u64) -> Option<PageSnapshot> {
        self.diagnose_calls.fetch_add(1, Ordering::SeqCst);
        if self.diagnose_hangs {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        self.diagnose_snap.lock().unwrap().clone()
    }
    fn live_limits(&self) -> (u64, Duration) {
        self.live_limits.unwrap_or((LIVE_SECS, LIVE_LAUNCH_ALLOWANCE))
    }
    async fn live_snapshot(&self, _url: &str, actions: &[LiveAction], secs: u64) -> Option<(PageSnapshot, LiveRun, Vec<String>)> {
        assert!(!self.live_panics, "the ladder asked for a live page load");
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        self.live_seen.lock().unwrap().push(actions.to_vec());
        self.live_secs_seen.lock().unwrap().push(secs);
        if self.live_hangs {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        self.live_result.lock().unwrap().clone()
    }
    fn model(&self) -> Option<&dyn RepairModel> {
        self.model.as_ref().map(|m| m as &dyn RepairModel)
    }
    fn fetcher(&self) -> &dyn Fetcher {
        &self.fetch
    }
    fn prober(&self) -> &dyn Prober {
        self
    }
    fn resolver(&self) -> &dyn Resolver {
        self
    }
    fn is_ad(&self, _url: &str) -> bool {
        false
    }
    async fn host_is_public(&self, _host: &str, _port: u16) -> bool {
        self.host_public.load(Ordering::SeqCst)
    }
    fn card_tools(&self) -> Option<(PathBuf, &dyn ImageSource)> {
        self.card_ffmpeg.clone().map(|f| (f, &self.pics as &dyn ImageSource))
    }
    fn card_cap(&self) -> Duration {
        self.card_cap.unwrap_or(card::CARD_CAP)
    }
    async fn download(
        &self,
        job: Job,
        _owner: &str,
        referer: Option<String>,
        _cancel: &CancellationToken,
        mode: DeliveryMode,
    ) -> Result<RunOutcome> {
        self.downloads.lock().unwrap().push((job.url.clone(), referer, mode));
        match self.script.lock().unwrap().pop_front() {
            Some(Ok(o)) => Ok(o),
            Some(Err(code)) => Err(anyhow::anyhow!("scripted failure").context(code.as_str())),
            None => Err(anyhow::anyhow!("no scripted outcome").context(ErrorCode::PipelineFailed.as_str())),
        }
    }
}

/// Any call is a failure of the test: the ladder must not have been reached.
struct PanicEnv;
#[async_trait]
impl Env for PanicEnv {
    async fn diagnose(&self, _: &str, _: u64) -> Option<PageSnapshot> {
        panic!("the ladder called diagnose")
    }
    async fn live_snapshot(&self, _: &str, _: &[LiveAction], _: u64) -> Option<(PageSnapshot, LiveRun, Vec<String>)> {
        panic!("the ladder asked for a live page load")
    }
    fn fetcher(&self) -> &dyn Fetcher {
        panic!("the ladder asked for the fetcher")
    }
    fn prober(&self) -> &dyn Prober {
        panic!("the ladder asked for the prober")
    }
    fn resolver(&self) -> &dyn Resolver {
        panic!("the ladder asked for the resolver")
    }
    fn is_ad(&self, _: &str) -> bool {
        panic!("the ladder checked an ad")
    }
    async fn host_is_public(&self, _: &str, _: u16) -> bool {
        panic!("the ladder resolved a host")
    }
    async fn download(&self, _: Job, _: &str, _: Option<String>, _: &CancellationToken, _: DeliveryMode) -> Result<RunOutcome> {
        panic!("the ladder downloaded")
    }
}

struct Rig {
    _dir: Scratch,
    repo: Repository,
    job: Job,
    ctx: RepairCtx,
    fake: Arc<Fake>,
}

fn cfg(mode: RepairMode, budget: Duration) -> RepairCfg {
    RepairCfg { mode, budget, hold_hours: 24, held_max_bytes: 20 << 30, llm: LlmCfg::default() }
}

fn leased_job(repo: &Repository) -> Job {
    repo.enqueue(&NewJob::new(PAGE, "1_ANNA_TEST", "ANNA"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap();
    repo.lease_job(OWNER, 600).unwrap().expect("leased")
}

fn rig_with(mode: RepairMode, budget: Duration, fake: Fake) -> Rig {
    let dir = Scratch::new();
    let repo = Repository::new(dir.0.join("omni.db")).unwrap();
    repo.sync_shipped(&recipe::shipped_for_store(), Utc::now()).unwrap();
    let job = leased_job(&repo);
    let fake = Arc::new(fake);
    let ctx = RepairCtx { repo: repo.clone(), cfg: cfg(mode, budget), env: fake.clone() };
    Rig { _dir: dir, repo, job, ctx, fake }
}

fn rig(mode: RepairMode) -> Rig {
    rig_with(mode, Duration::from_secs(60), Fake::new())
}

fn page(html: &str) -> PageSnapshot {
    let mut s = PageSnapshot::new(PAGE);
    s.html = Some(html.to_string());
    s
}

fn next_data(url: &str) -> String {
    format!(r#"<html><body><script id="__NEXT_DATA__" type="application/json">{{"props":{{"video":"{url}"}}}}</script></body></html>"#)
}

fn og(url: &str) -> String {
    format!(r#"<meta property="og:video" content="{url}">"#)
}

async fn go(r: &Rig, snap: Option<PageSnapshot>, prior: &Prior) -> LadderResult {
    run(&r.ctx, &r.job, OWNER, PAGE, snap, prior, &CancellationToken::new()).await
}

fn events(r: &Rig) -> Vec<String> {
    r.repo.get_job_events(r.job.id, 500).unwrap().into_iter().map(|e| e.message).collect()
}

fn shipped_hash(name: &str) -> String {
    let (n, rec) = recipe::shipped().into_iter().find(|(n, _)| *n == name).unwrap();
    recipe::scope_hash("generic", n, &rec)
}

/// Recipes of the store that have a row on the page's domain: the ones that got an event.
fn with_rows(r: &Rig) -> usize {
    r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap().iter().filter(|x| x.site_state.is_some()).count()
}

#[tokio::test]
async fn shadow_records_what_it_would_use_and_never_downloads() {
    let r = rig(RepairMode::Shadow);
    let LadderResult::ShadowFound { verified } = go(&r, Some(page(&next_data(A))), &Prior::default()).await else {
        panic!("expected a shadow find")
    };
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0].candidate.url, A);
    assert_eq!(verified[0].candidate.referer.as_deref(), Some(PAGE));
    assert!(r.fake.downloads().is_empty(), "shadow never downloads");
    let hash = shipped_hash("next_data_media");
    let ev = events(&r);
    let want = format!("Η αυτόματη επισκευή θα χρησιμοποιούσε: cdn.news.example.gr (02:00, recipe:{})", &hash[..8]);
    assert!(ev.iter().any(|m| *m == want), "{ev:?}");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.mode.as_str(), at.rung.as_deref(), at.outcome.as_deref()), ("shadow", Some("recipes"), Some("shadow_found")));
    assert_eq!(at.recipe_hash.as_deref(), Some(hash.as_str()), "the FULL recipe hash is recorded");
    assert_eq!(at.domain.as_deref(), Some(DOMAIN));
    let list: serde_json::Value = serde_json::from_str(at.candidates_json.as_deref().unwrap()).unwrap();
    assert_eq!(list[0]["url"], A);
    assert!(list[0]["checks"].as_array().unwrap().iter().any(|c| c["name"] == "probe" && c["ok"] == true));
    assert_eq!(r.repo.get_job(r.job.id).unwrap().unwrap().extraction_method, None, "shadow changes nothing on the job");
}

#[tokio::test]
async fn hold_downloads_the_best_verified_video_held_and_marks_the_method() {
    let r = rig(RepairMode::Hold);
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let res = go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    let hash = shipped_hash("next_data_media");
    let LadderResult::Held { recipe_hash } = res else { panic!("expected Held, got {res:?}") };
    assert_eq!(recipe_hash, hash);
    let d = r.fake.downloads();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].0, A);
    assert_eq!(d[0].1.as_deref(), Some(PAGE));
    assert_eq!(d[0].2, DeliveryMode::Hold { hold_hours: 24, held_max_bytes: 20 << 30 }, "always held, never delivered");
    let job = r.repo.get_job(r.job.id).unwrap().unwrap();
    assert_eq!(job.extraction_method, Some(format!("repair:recipe:{}", &hash[..8])));
    assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().outcome.as_deref(), Some("held"));
    // The ladder never sets a status: the worker does.
    assert_eq!(job.status, JobStatus::Running);
}

/// Three recipes, three different videos, in rank order a, b, c.
fn three() -> PageSnapshot {
    let mut s = page(&format!("{}{}", og(A), next_data(B)));
    s.ld_json.push(format!(r#"{{"@type":"VideoObject","contentUrl":"{C}"}}"#));
    s
}

#[tokio::test]
async fn a_failed_first_download_tries_the_second_candidate_once() {
    let r = rig(RepairMode::Hold);
    {
        let mut s = r.fake.script.lock().unwrap();
        s.push_back(Err(ErrorCode::Http403));
        s.push_back(Ok(RunOutcome::Held));
    }
    let res = go(&r, Some(three()), &Prior::default()).await;
    let LadderResult::Held { recipe_hash } = res else { panic!("expected Held, got {res:?}") };
    assert_eq!(recipe_hash, shipped_hash("next_data_media"), "the hash of the recipe behind the video that was held");
    let urls: Vec<String> = r.fake.downloads().into_iter().map(|d| d.0).collect();
    assert_eq!(urls, vec![A.to_string(), B.to_string()]);
    assert!(
        events(&r).iter().any(|m| m.contains("η λήψη από cdn.news.example.gr απέτυχε (HTTP_403)")),
        "{:?}",
        events(&r)
    );
    // The records name candidate #2 and its recipe, not the one that failed first.
    let second = shipped_hash("next_data_media");
    let first = shipped_hash("og_video");
    let job = r.repo.get_job(r.job.id).unwrap().unwrap();
    assert_eq!(job.extraction_method, Some(format!("repair:recipe:{}", &second[..8])));
    assert_ne!(&first[..8], &second[..8]);
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!(at.recipe_hash.as_deref(), Some(second.as_str()), "the recipe behind the held video");
    assert_eq!(at.held_url.as_deref(), Some(B));
    assert_eq!(at.next_index, 1);
    let rows = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap();
    let id_of = |hash: &str| rows.iter().find(|x| x.hash == hash).unwrap().id;
    assert_eq!(at.recipe_id, Some(id_of(&second)));
    assert_ne!(at.recipe_id, Some(id_of(&first)));
}

#[tokio::test]
async fn two_failed_downloads_end_in_nothing_verified_and_never_a_third() {
    let r = rig(RepairMode::Hold);
    {
        let mut s = r.fake.script.lock().unwrap();
        s.push_back(Err(ErrorCode::Http403));
        s.push_back(Err(ErrorCode::DownloadTimeout));
        s.push_back(Ok(RunOutcome::Held));
    }
    let res = go(&r, Some(three()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.downloads().len(), 2, "never more than two downloads, with three verified");
    let ev = events(&r);
    assert!(ev.iter().any(|m| m.contains("HTTP_403")) && ev.iter().any(|m| m.contains("DOWNLOAD_TIMEOUT")), "{ev:?}");
    assert_eq!(r.repo.get_job(r.job.id).unwrap().unwrap().extraction_method, None, "the repair mark is taken back");
    assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().outcome.as_deref(), Some("download_failed"));
}

#[tokio::test]
async fn no_candidates_is_nothing_verified_and_every_recipe_that_ran_records_a_miss() {
    let r = rig(RepairMode::Hold);
    assert_eq!(with_rows(&r), 0);
    let res = go(&r, Some(page("<html><body><p>no video here</p></body></html>")), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.probes(), 0);
    assert!(r.fake.downloads().is_empty());
    assert_eq!(with_rows(&r), recipe::shipped().len(), "each shipped recipe that found nothing got a NoCandidate event");
    // A page with a video gives the recipe that found it no event.
    let r = rig(RepairMode::Shadow);
    let res = go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }));
    assert_eq!(with_rows(&r), recipe::shipped().len() - 1, "the recipe behind a verified candidate records nothing here");
    let rows = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap();
    let hit = rows.iter().find(|x| x.hash == shipped_hash("next_data_media")).unwrap();
    assert!(hit.site_state.is_none());
}

#[tokio::test]
async fn a_recipe_whose_every_candidate_fails_the_gate_records_a_miss() {
    let fake = Fake::new();
    fake.probe.lock().unwrap().insert(A.to_string(), Err("ffprobe could not read the address".to_string()));
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(60), fake);
    let res = go(&r, Some(page(&format!("{}{}", og(A), next_data(B)))), &Prior::default()).await;
    let LadderResult::ShadowFound { verified } = res else { panic!("expected a find") };
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0].candidate.url, B);
    let rows = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap();
    let state = |name: &str| rows.iter().find(|x| x.hash == shipped_hash(name)).unwrap().site_state;
    assert!(state("og_video").is_some(), "its only candidate was rejected by the gate");
    assert!(state("next_data_media").is_none(), "its candidate verified");
}

/// A recipe whose candidate the gate never looked at (a better video was
/// accepted first) has not failed: no miss is recorded for it.
#[tokio::test]
async fn a_candidate_the_gate_never_probed_gives_its_recipe_no_miss() {
    let r = rig(RepairMode::Shadow);
    let mut snap = three();
    snap.html = Some(format!("{}{}", snap.html.unwrap(), format!(r#"<meta property="og:video:url" content="{D}">"#)));
    let res = go(&r, Some(snap), &Prior::default()).await;
    let LadderResult::ShadowFound { verified } = res else { panic!("expected a find, got {res:?}") };
    assert_eq!(verified.len(), 3);
    assert_eq!(r.fake.probes(), 3, "the fourth candidate was never probed");
    let rows = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap();
    let state = |name: &str| rows.iter().find(|x| x.hash == shipped_hash(name)).unwrap().site_state;
    assert!(state("og_video_url").is_none(), "not probed, so not a miss");
    for verified_recipe in ["og_video", "next_data_media", "ldjson_content_url"] {
        assert!(state(verified_recipe).is_none(), "{verified_recipe} verified");
    }
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!(at.outcome.as_deref(), Some("shadow_found"));
}

#[tokio::test]
async fn the_budget_bounds_the_search_and_ends_in_exhausted() {
    let mut fake = Fake::new();
    fake.probe_delay = Duration::from_secs(30);
    let r = rig_with(RepairMode::Hold, Duration::from_secs(1), fake);
    let t = Instant::now();
    let res = go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Exhausted), "{res:?}");
    assert!(t.elapsed() < Duration::from_secs(6), "budget 1 s + 5 s, took {:?}", t.elapsed());
    assert!(r.fake.downloads().is_empty());
    assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().outcome.as_deref(), Some("exhausted"));
    assert_eq!(with_rows(&r), recipe::shipped().len() - 1, "a timeout is not a miss for the recipe that found the video");

    // A look at the page that never ends is bounded the same way.
    let mut fake = Fake::new();
    fake.diagnose_hangs = true;
    let r = rig_with(RepairMode::Hold, Duration::from_secs(1), fake);
    let t = Instant::now();
    let res = go(&r, None, &Prior::default()).await;
    assert!(matches!(res, LadderResult::Exhausted), "{res:?}");
    assert!(t.elapsed() < Duration::from_secs(6), "took {:?}", t.elapsed());
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn without_page_text_one_diagnostic_look_supplies_it() {
    let r = rig(RepairMode::Shadow);
    *r.fake.diagnose_snap.lock().unwrap() = Some(page(&next_data(A)));
    // The sniff left a snapshot without html (a stream that then 403'd).
    let bare = PageSnapshot::new(PAGE);
    let res = go(&r, Some(bare), &Prior::default()).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }), "{res:?}");
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 1);
    // A snapshot that has its html is not looked at again.
    let r = rig(RepairMode::Shadow);
    go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    assert_eq!(r.fake.diagnose_calls.load(Ordering::SeqCst), 0);
    // And no page at all is nothing to repair, not a guess.
    let r = rig(RepairMode::Hold);
    let res = go(&r, None, &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(with_rows(&r), 0, "no page, so no recipe was tried");
}

#[tokio::test]
async fn cancel_ends_the_ladder_at_once_and_writes_nothing() {
    let mut fake = Fake::new();
    fake.probe_delay = Duration::from_secs(30);
    let r = rig_with(RepairMode::Hold, Duration::from_secs(60), fake);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let res = run(&r.ctx, &r.job, OWNER, PAGE, Some(page(&next_data(A))), &Prior::default(), &cancel).await;
    assert!(matches!(res, LadderResult::Lost), "{res:?}");
    assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    assert!(r.fake.downloads().is_empty());
    assert!(events(&r).iter().all(|m| !m.contains("επισκευή")), "{:?}", events(&r));
    assert_eq!(r.repo.get_job(r.job.id).unwrap().unwrap().extraction_method, None);
}

#[tokio::test]
async fn a_lost_lease_ends_the_ladder_before_any_work_or_write() {
    let r = rig(RepairMode::Hold);
    let res = run(&r.ctx, &r.job, "someone-else", PAGE, Some(page(&next_data(A))), &Prior::default(), &CancellationToken::new()).await;
    assert!(matches!(res, LadderResult::Lost), "{res:?}");
    assert_eq!(r.fake.probes(), 0);
    assert!(r.fake.downloads().is_empty());
    assert_eq!(with_rows(&r), 0, "no recipe event for a job that is not ours");
    assert!(events(&r).iter().all(|m| !m.contains("επισκευή")), "{:?}", events(&r));
}

#[tokio::test]
async fn a_video_the_normal_path_already_tried_is_dropped_unless_the_referer_differs() {
    let tried = |referer: Option<&str>| Prior {
        tried: vec![Tried { url: A.to_string(), referer: referer.map(str::to_string) }],
        ..Default::default()
    };
    let r = rig(RepairMode::Shadow);
    let res = go(&r, Some(page(&next_data(A))), &tried(Some(PAGE))).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.probes(), 0, "the same address with the same referer is not tried again");
    let rows = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap();
    let hit = rows.iter().find(|x| x.hash == shipped_hash("next_data_media")).unwrap();
    assert!(hit.site_state.is_none(), "a candidate dropped because it was already tried is no miss for its recipe");
    // The same address, tried by the sniffer with another referer, is a different request.
    let r = rig(RepairMode::Shadow);
    let res = go(&r, Some(page(&next_data(A))), &tried(Some("https://other.example/"))).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }), "{res:?}");
    // The match is on the normalized address.
    let r = rig(RepairMode::Shadow);
    let p = Prior { tried: vec![Tried { url: format!("{A}#t=0"), referer: Some(PAGE.to_string()) }], ..Default::default() };
    let res = go(&r, Some(page(&next_data(A))), &p).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    // A video MCR rejected is never offered again, whatever the referer.
    let r = rig(RepairMode::Shadow);
    let p = Prior { rejected_by_mcr: vec![A.to_string()], ..Default::default() };
    let res = go(&r, Some(page(&next_data(A))), &p).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.probes(), 0);
    let rows = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap();
    let hit = rows.iter().find(|x| x.hash == shipped_hash("next_data_media")).unwrap();
    assert!(hit.site_state.is_none(), "MCR's rejection is recorded by the desk, not as a miss here");
}

#[tokio::test]
async fn an_unobserved_or_private_host_never_reaches_the_prober() {
    for bad in [
        "https://evil.example.org/a.mp4",
        "http://10.0.0.5/a.mp4",
        "http://169.254.169.254/a.mp4",
        "http://localhost/a.mp4",
        "http://[::1]/a.mp4",
        "https://cdn.news.example.gr:8443/a.mp4",
    ] {
        let r = rig(RepairMode::Hold);
        let res = go(&r, Some(page(&next_data(bad))), &Prior::default()).await;
        assert!(matches!(res, LadderResult::NothingVerified), "{bad}: {res:?}");
        assert_eq!(r.fake.probes(), 0, "{bad} reached the prober");
        assert!(r.fake.downloads().is_empty(), "{bad}");
    }
    // The page's own host is fine (control: the prober is reached).
    let r = rig(RepairMode::Hold);
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    assert_eq!(r.fake.probes(), 1);

    // A host the page used, but whose name now points inside: stopped at the gate.
    let mut snap = page(&next_data("https://cdn.other-cdn.net/a.mp4"));
    snap.network.push(NetworkEntry { url: "https://cdn.other-cdn.net/player.js".into(), ..Default::default() });
    let r = rig(RepairMode::Shadow);
    r.fake.resolves_to.lock().unwrap().insert("cdn.other-cdn.net".into(), "10.1.2.3".parse().unwrap());
    let res = go(&r, Some(snap.clone()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.probes(), 0, "a name that resolves to a private address is not probed");
    let r = rig(RepairMode::Shadow);
    let res = go(&r, Some(snap), &Prior::default()).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }), "{res:?}");
}

#[tokio::test]
async fn an_explicit_port_passes_only_when_the_page_itself_used_it() {
    let url = "https://cdn.news.example.gr:8443/a.mp4";
    let mut snap = page(&next_data(url));
    snap.network.push(NetworkEntry { url: "https://cdn.news.example.gr:8443/seg-1.ts".into(), ..Default::default() });
    let r = rig(RepairMode::Shadow);
    let res = go(&r, Some(snap), &Prior::default()).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }), "the gate gets host:port from the network log: {res:?}");
}

#[tokio::test]
async fn the_host_is_resolved_again_just_before_the_download() {
    let r = rig(RepairMode::Hold);
    r.fake.host_public.store(false, Ordering::SeqCst);
    let res = go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert!(r.fake.downloads().is_empty(), "a host that turned private is never downloaded from");
    assert!(events(&r).iter().any(|m| m.contains("δεν δείχνει πια σε δημόσια διεύθυνση")));
}

#[tokio::test]
async fn the_pipeline_gets_the_serialized_address_the_gate_verified() {
    let r = rig(RepairMode::Hold);
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    // Uppercase host and a dot segment: the gate's own form is what is downloaded.
    let raw = "https://CDN.News.Example.gr/v/../v/a.mp4";
    let res = go(&r, Some(page(&next_data(raw))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.downloads()[0].0, "https://cdn.news.example.gr/v/a.mp4");
}

#[test]
fn jobs_are_classified_for_repair() {
    let dir = Scratch::new();
    let repo = Repository::new(dir.0.join("omni.db")).unwrap();
    let base = leased_job(&repo);
    let mk = |url: &str| {
        let mut j = base.clone();
        j.url = url.to_string();
        j
    };
    assert_eq!(job_kind(&mk(PAGE), PAGE), JobKind::WebPage);
    assert_eq!(job_kind(&mk("attachment://m1@example.gr/1"), "attachment://m1@example.gr/1"), JobKind::Attachment);
    assert_eq!(job_kind(&mk("https://wetransfer.com/downloads/abc"), "https://wetransfer.com/downloads/abc"), JobKind::Locker);
    let mut locker = mk(PAGE);
    locker.extraction_method = Some("locker".into());
    assert_eq!(job_kind(&locker, PAGE), JobKind::Locker);
    assert_eq!(job_kind(&mk("https://www.youtube.com/watch?v=abc"), "https://www.youtube.com/watch?v=abc"), JobKind::Platform);
    let mut sibling = mk(PAGE);
    sibling.parent_job_id = Some(7);
    assert_eq!(job_kind(&sibling, PAGE), JobKind::ArticleSibling);
    assert_eq!(job_kind(&mk("ftp://x/y"), "ftp://x/y"), JobKind::Other);
}

/// The ladder is reached only for an unresolved video on a web page with
/// repair on. The env panics on any call and no attempt may be recorded.
#[tokio::test]
async fn guarantee_no_other_failure_ever_calls_the_ladder() {
    let dir = Scratch::new();
    let repo = Repository::new(dir.0.join("omni.db")).unwrap();
    let job = leased_job(&repo);
    let cancel = CancellationToken::new();
    let snap = Some(page(&next_data(A)));
    let mut asked = 0;
    for mode in [RepairMode::Off, RepairMode::Shadow, RepairMode::Hold] {
        let ctx = RepairCtx { repo: repo.clone(), cfg: cfg(mode, Duration::from_secs(5)), env: Arc::new(PanicEnv) };
        for code in ErrorCode::ALL {
            if mode != RepairMode::Off && code == ErrorCode::VideoUnresolved {
                continue; // the one eligible shape is covered by the tests above
            }
            let tried = Tried::of(&job, PAGE, None, &[], None);
            let got = after_failure(&ctx, &job, OWNER, PAGE, code, &snap, tried, &cancel).await;
            assert!(got.is_none(), "{code:?} in {mode:?} reached the ladder");
            asked += 1;
        }
    }
    assert_eq!(asked, 3 * ErrorCode::ALL.len() - 2);
    assert!(repo.latest_attempt(job.id).unwrap().is_none(), "nothing was recorded");

    // Right code, wrong kind of job.
    let ctx = RepairCtx { repo: repo.clone(), cfg: cfg(RepairMode::Hold, Duration::from_secs(5)), env: Arc::new(PanicEnv) };
    for url in ["attachment://m1@example.gr/1", "https://wetransfer.com/downloads/abc", "https://www.youtube.com/watch?v=abc"] {
        let mut j = job.clone();
        j.url = url.to_string();
        let got = after_failure(&ctx, &j, OWNER, url, ErrorCode::VideoUnresolved, &snap, vec![], &cancel).await;
        assert!(got.is_none(), "{url}");
    }
    let mut sibling = job.clone();
    sibling.parent_job_id = Some(99);
    assert!(after_failure(&ctx, &sibling, OWNER, PAGE, ErrorCode::VideoUnresolved, &snap, vec![], &cancel).await.is_none());
    // A cancelled job, or one that is no longer ours.
    let stop = CancellationToken::new();
    stop.cancel();
    assert!(after_failure(&ctx, &job, OWNER, PAGE, ErrorCode::VideoUnresolved, &snap, vec![], &stop).await.is_none());
    assert!(after_failure(&ctx, &job, "someone-else", PAGE, ErrorCode::VideoUnresolved, &snap, vec![], &cancel).await.is_none());
    assert!(repo.latest_attempt(job.id).unwrap().is_none());
}

#[tokio::test]
async fn guarantee_repair_off_never_calls_the_ladder_even_for_an_unresolved_video() {
    let dir = Scratch::new();
    let repo = Repository::new(dir.0.join("omni.db")).unwrap();
    let job = leased_job(&repo);
    let ctx = RepairCtx { repo: repo.clone(), cfg: cfg(RepairMode::Off, Duration::from_secs(5)), env: Arc::new(PanicEnv) };
    let snap = Some(page(&next_data(A)));
    let got = after_failure(&ctx, &job, OWNER, PAGE, ErrorCode::VideoUnresolved, &snap, vec![], &CancellationToken::new()).await;
    assert!(got.is_none());
    assert!(repo.latest_attempt(job.id).unwrap().is_none());
    // `run` itself refuses too.
    let res = run(&ctx, &job, OWNER, PAGE, None, &Prior::default(), &CancellationToken::new()).await;
    assert!(matches!(res, LadderResult::NothingVerified));
    assert!(repo.latest_attempt(job.id).unwrap().is_none());
}

/// The eligible shape does reach the ladder (control for the guarantees above).
#[tokio::test]
async fn the_eligible_shape_reaches_the_ladder() {
    let r = rig(RepairMode::Shadow);
    let snap = Some(page(&next_data(A)));
    let got = after_failure(&r.ctx, &r.job, OWNER, PAGE, ErrorCode::VideoUnresolved, &snap, vec![], &CancellationToken::new()).await;
    assert!(matches!(got, Some(LadderResult::ShadowFound { .. })), "{got:?}");
    assert!(r.repo.latest_attempt(r.job.id).unwrap().is_some());
}

/// A page where three recipes each name a video (og:video, __NEXT_DATA__, ld+json),
/// so none of them records a miss from one run to the next.
fn triple(o: &str, n: &str, l: &str) -> PageSnapshot {
    let mut s = page(&format!("{}{}", og(o), next_data(n)));
    s.ld_json.push(format!(r#"{{"@type":"VideoObject","contentUrl":"{l}"}}"#));
    s
}

/// Lease the job again after MCR's "try again" and give the ladder a page.
async fn rerun(r: &Rig, snap: PageSnapshot) -> Option<LadderResult> {
    assert!(r.repo.lease_job_by_id(r.job.id, OWNER, 600).unwrap(), "the retried job is leasable");
    let job = r.repo.get_job(r.job.id).unwrap().unwrap();
    assert_eq!(job.error_code, None, "MCR's retry clears the error code; the ladder must not rely on it");
    after_failure(&r.ctx, &job, OWNER, PAGE, ErrorCode::VideoUnresolved, &Some(snap), vec![], &CancellationToken::new()).await
}

/// What the worker and the desk do around a held video, with the real
/// repository functions: the file is held, the job parked, then MCR decides.
fn park(r: &Rig) {
    assert!(r.repo.hold_output(r.job.id, OWNER, "temp/held/x/clip.mxf", 10, &"ab".repeat(32), Some(9.0), Utc::now() + chrono::Duration::hours(24)).unwrap());
    assert!(crate::finish_held(&r.repo, r.job.id, OWNER).unwrap());
}

fn held_urls(r: &Rig) -> Vec<String> {
    r.fake.downloads().into_iter().map(|d| d.0).collect()
}

/// hold -> reject_held -> retry_job -> ladder: the rejected address is not held
/// again, with only that candidate and with a second one; a second rejection
/// drops both. Through the real repository flow, which clears `error_code`.
#[tokio::test]
async fn guarantee_a_video_mcr_rejected_is_not_held_again_after_a_retry() {
    let r = rig(RepairMode::Hold);
    let id = r.job.id;
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let got = after_failure(&r.ctx, &r.job, OWNER, PAGE, ErrorCode::VideoUnresolved, &Some(triple(A, A, A)), vec![], &CancellationToken::new()).await;
    assert!(matches!(got, Some(LadderResult::Held { .. })), "{got:?}");
    park(&r);
    assert!(r.repo.reject_held(id, "wrong", "not the story", "desk").unwrap());
    assert!(r.repo.retry_job(id, None).unwrap());

    // Only the rejected video on the page: nothing to hold.
    let got = rerun(&r, triple(A, A, A)).await;
    assert!(matches!(got, Some(LadderResult::NothingVerified)), "{got:?}");
    assert_eq!(held_urls(&r), vec![A.to_string()], "the rejected video was not downloaded again");
    r.repo.finish(id, OWNER, JobStatus::RequiresReview, Some("VIDEO_UNRESOLVED"), None, None).unwrap();
    assert!(r.repo.retry_job(id, None).unwrap());

    // A second video on the page: that one is held.
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let got = rerun(&r, triple(A, B, A)).await;
    assert!(matches!(got, Some(LadderResult::Held { .. })), "{got:?}");
    assert_eq!(held_urls(&r), vec![A.to_string(), B.to_string()]);
    park(&r);
    assert!(r.repo.reject_held(id, "wrong", "not the story", "desk").unwrap());
    assert!(r.repo.retry_job(id, None).unwrap());

    // Both rejected: only a third video is offered.
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let got = rerun(&r, triple(A, B, C)).await;
    assert!(matches!(got, Some(LadderResult::Held { .. })), "{got:?}");
    assert_eq!(held_urls(&r), vec![A.to_string(), B.to_string(), C.to_string()], "A and B were both dropped");
    assert_eq!(r.repo.rejected_held_urls(id).unwrap(), vec![A.to_string(), B.to_string()]);
}

/// An expired hold is not a rejection: the same video may be held again.
#[tokio::test]
async fn an_expired_hold_is_not_treated_as_a_rejection() {
    let r = rig(RepairMode::Hold);
    let id = r.job.id;
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let got = after_failure(&r.ctx, &r.job, OWNER, PAGE, ErrorCode::VideoUnresolved, &Some(page(&next_data(A))), vec![], &CancellationToken::new()).await;
    assert!(matches!(got, Some(LadderResult::Held { .. })), "{got:?}");
    park(&r);
    assert!(r.repo.expire_held(id, Utc::now()).unwrap());
    assert!(r.repo.retry_job(id, None).unwrap());
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let got = rerun(&r, page(&next_data(A))).await;
    assert!(matches!(got, Some(LadderResult::Held { .. })), "{got:?}");
    assert_eq!(held_urls(&r), vec![A.to_string(), A.to_string()]);
}

/// A rooted host name ("host.") is refused before the download: the address pin
/// is keyed on the name without the dot.
#[tokio::test]
async fn a_rooted_host_name_is_never_downloaded() {
    let r = rig(RepairMode::Hold);
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let res = go(&r, Some(page(&next_data("https://cdn.news.example.gr./v/a.mp4"))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert!(r.fake.downloads().is_empty());
}

#[test]
fn every_ladder_answer_in_every_mode_has_its_disposition() {
    let budget = Duration::from_secs(180);
    let results = || {
        vec![
            LadderResult::Held { recipe_hash: "ab".repeat(32) },
            LadderResult::ShadowFound { verified: vec![] },
            LadderResult::NothingVerified,
            LadderResult::Exhausted,
            LadderResult::Lost,
        ]
    };
    for mode in [RepairMode::Off, RepairMode::Shadow, RepairMode::Hold] {
        for res in results() {
            let got = disposition(&res, mode, budget);
            let want = match (&res, mode) {
                (LadderResult::Held { .. }, _) => Disposition::Held,
                (LadderResult::Lost, _) => Disposition::Lost,
                (LadderResult::Exhausted, RepairMode::Hold) => {
                    Disposition::Finish { code: ErrorCode::RepairExhausted, message: "Η αυτόματη επισκευή δεν ολοκληρώθηκε μέσα σε 180 δευτερόλεπτα".into() }
                }
                _ => Disposition::Unchanged,
            };
            assert_eq!(got, want, "{res:?} in {mode:?}");
        }
    }
    // Shadow never changes how a job ends, not even for a spent budget.
    assert_eq!(disposition(&LadderResult::Exhausted, RepairMode::Shadow, budget), Disposition::Unchanged);
}

#[test]
fn a_repair_that_ran_out_of_time_counts_for_the_site_as_an_unresolved_video() {
    assert_eq!(site_code(ErrorCode::RepairExhausted), ErrorCode::VideoUnresolved);
    for code in ErrorCode::ALL {
        if code != ErrorCode::RepairExhausted {
            assert_eq!(site_code(code), code);
        }
    }
    assert!(omni_core::site_health::counts_against_site(site_code(ErrorCode::RepairExhausted).as_str()));
}

/// The run of the browser is not dropped when the ladder stops waiting.
#[tokio::test]
async fn detached_work_finishes_even_when_the_waiter_gives_up() {
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    let waited = tokio::time::timeout(
        Duration::from_millis(50),
        detached(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            d.store(true, Ordering::SeqCst);
            7
        }),
    )
    .await;
    assert!(waited.is_err(), "the waiter gave up");
    assert!(!done.load(Ordering::SeqCst));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(done.load(Ordering::SeqCst), "the work ran to its own end, closing what it opened");
    assert_eq!(detached(async { 7 }).await, Some(7));
}

/// Recipe evaluation time on a big invented page: all shipped recipes, one at
/// a time under the process-wide permit, inside a budget.
#[tokio::test]
async fn the_whole_recipe_pass_on_a_multi_megabyte_page_stays_inside_the_budget() {
    let filler: String = (0..14000)
        .map(|i| format!("<div class=\"c{i}\" data-x=\"v\"><span>lorem ipsum dolor sit amet {i}</span></div>"))
        .collect();
    let junk = "x".repeat(900 * 1024);
    let html = format!(
        "<html><head><meta property=\"og:title\" content=\"t\"></head><body>{filler}\
         <script>window.__NUXT__={{a:\"{junk}\"}}; var sources: [ {{ nothing: \"{junk}\" }} ];</script>\
         <script id=\"__NEXT_DATA__\" type=\"application/json\">{{\"junk\":\"{junk}\",\"video\":\"{A}\"}}</script>\
         {filler}</body></html>"
    );
    assert!(html.len() > 3 << 20, "{} bytes", html.len());
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(60), Fake::new());
    let t = Instant::now();
    let res = go(&r, Some(page(&html)), &Prior::default()).await;
    let took = t.elapsed();
    eprintln!("recipe pass over {} MiB of html, {} recipes: {took:?}", html.len() >> 20, recipe::shipped().len());
    assert!(matches!(res, LadderResult::ShadowFound { .. } | LadderResult::NothingVerified), "{res:?}");
    assert!(took < Duration::from_secs(20), "the recipe pass took {took:?}");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert!(at.duration_ms.unwrap() < 20_000);
}

// ------------------------------------------------ the approval card's evidence (P10.11b)

const PIC: &str = "https://cdn.news.example.gr/img/p.jpg";

/// A held row for the job and its directory, as the pipeline leaves them.
fn held_dir_for(r: &Rig) -> PathBuf {
    let dir = r._dir.0.join("held").join(r.job.id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let mxf = dir.join("1_ANNA_TEST.mxf");
    std::fs::write(&mxf, b"not a real mxf").unwrap();
    let expires = Utc::now() + chrono::Duration::hours(24);
    assert!(r.repo.hold_output(r.job.id, OWNER, &mxf.to_string_lossy(), 14, "abc", Some(120.0), expires).unwrap());
    dir
}

/// A page with the shipped recipe's video, the page's stated length (2 min) and one article picture.
fn page_with_picture() -> PageSnapshot {
    let mut s = page(&next_data(A));
    s.ld_json.push(r#"{"@type":"VideoObject","duration":"PT2M"}"#.to_string());
    s.posters = vec![PIC.to_string()];
    s
}

fn hold_rig(card_ffmpeg: Option<PathBuf>, body: Option<Vec<u8>>) -> Rig {
    let mut fake = Fake::new();
    fake.card_ffmpeg = card_ffmpeg;
    *fake.pics.body.lock().unwrap() = body;
    let r = rig_with(RepairMode::Hold, Duration::from_secs(60), fake);
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    r
}

/// A 4 s `_preview.mp4` of a moving picture and one frame of it as a PNG body.
async fn preview_and_picture(ffmpeg: &Path, dir: &Path) -> Vec<u8> {
    let run = |args: Vec<String>| async move {
        let o = omni_core::process::run(ffmpeg, &args, omni_core::process::RunOpts::new(Duration::from_secs(60))).await.unwrap();
        assert!(o.success, "{}", o.stderr_tail);
    };
    let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let preview = dir.join("_preview.mp4").to_string_lossy().into_owned();
    let mut a = s(&["-y", "-v", "error", "-f", "lavfi", "-i", "mandelbrot=size=640x360:rate=25", "-t", "4", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"]);
    a.push(preview);
    run(a).await;
    let png = dir.join("pic.png");
    let mut a = s(&["-y", "-v", "error", "-f", "lavfi", "-i", "mandelbrot=size=640x360:rate=25", "-ss", "2", "-frames:v", "1"]);
    a.push(png.to_string_lossy().into_owned());
    run(a).await;
    let bytes = std::fs::read(&png).unwrap();
    std::fs::remove_file(png).unwrap();
    bytes
}

#[tokio::test]
async fn a_held_run_records_the_picture_evidence_and_the_pages_stated_length_on_the_attempt() {
    let Some(ffmpeg) = tool("ffmpeg") else {
        eprintln!("WARNING: skipping the card evidence ladder test -- bin/ffmpeg.exe missing");
        return;
    };
    let r = hold_rig(Some(ffmpeg.clone()), None);
    let dir = held_dir_for(&r);
    *r.fake.pics.body.lock().unwrap() = Some(preview_and_picture(&ffmpeg, &dir).await);
    let res = go(&r, Some(page_with_picture()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.pics.calls.load(Ordering::SeqCst), 1, "card evidence runs once");
    let at = r.repo.latest_held_attempt(r.job.id).unwrap().unwrap();
    assert_eq!(at.image_host.as_deref(), Some("cdn.news.example.gr"));
    assert!(at.image_distance.is_some_and(|d| d <= 10), "{:?}", at.image_distance);
    assert!(at.image_frame_secs.is_some_and(|t| (t - 2.0).abs() <= 1.0), "{:?}", at.image_frame_secs);
    assert_eq!(at.expected_secs, Some(120.0));
    assert!(dir.join("_article.jpg").is_file() && dir.join("_frame.jpg").is_file());
    assert!(!dir.join("_article.src").exists());
}

#[tokio::test]
async fn a_card_evidence_failure_never_changes_the_hold() {
    // ffmpeg cannot even start, and a picture that is only a header: the step fails.
    for body in [Some([&[0xFF, 0xD8, 0xFF][..], &[1u8; 64][..]].concat()), None] {
        let r = hold_rig(Some(PathBuf::from("no-such-ffmpeg.exe")), body.clone());
        let dir = held_dir_for(&r);
        let res = go(&r, Some(page_with_picture()), &Prior::default()).await;
        let hash = shipped_hash("next_data_media");
        let LadderResult::Held { recipe_hash } = res else { panic!("expected Held, got {res:?}") };
        assert_eq!(recipe_hash, hash);
        assert_eq!(r.fake.pics.calls.load(Ordering::SeqCst), 1, "the step ran ({})", body.is_some());
        let at = r.repo.latest_held_attempt(r.job.id).unwrap().unwrap();
        assert_eq!((at.outcome.as_deref(), at.held_url.as_deref(), at.next_index), (Some("held"), Some(A), 0));
        assert_eq!(at.recipe_hash.as_deref(), Some(hash.as_str()));
        assert_eq!((at.image_distance, at.image_frame_secs, at.image_host), (None, None, None), "nothing written");
        assert_eq!(at.expected_secs, Some(120.0), "the page's length is recorded regardless");
        assert!(!dir.join("_article.jpg").exists() && !dir.join("_article.src").exists());
        assert!(r.repo.waiting_held(r.job.id).unwrap().is_some(), "the hold stands");
    }
}

/// The card step has its own cap; a picture that never arrives cannot hold the job.
#[tokio::test]
async fn a_picture_that_never_arrives_ends_at_the_card_cap_and_the_hold_stands() {
    let mut fake = Fake::new();
    fake.card_ffmpeg = Some(PathBuf::from("no-such-ffmpeg.exe"));
    fake.card_cap = Some(Duration::from_millis(800));
    fake.pics.hang.store(true, Ordering::SeqCst);
    let r = rig_with(RepairMode::Hold, Duration::from_secs(60), fake);
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let dir = held_dir_for(&r);
    let started = Instant::now();
    let res = go(&r, Some(page_with_picture()), &Prior::default()).await;
    assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
    let hash = shipped_hash("next_data_media");
    let LadderResult::Held { recipe_hash } = res else { panic!("expected Held, got {res:?}") };
    assert_eq!(recipe_hash, hash);
    assert_eq!(r.fake.pics.calls.load(Ordering::SeqCst), 1, "the step did start");
    let at = r.repo.latest_held_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.outcome.as_deref(), at.held_url.as_deref(), at.next_index), (Some("held"), Some(A), 0));
    assert_eq!((at.image_distance, at.image_frame_secs, at.image_host), (None, None, None));
    assert!(!dir.join("_card_work").exists() && !dir.join("_article.jpg").exists());
    assert!(r.repo.waiting_held(r.job.id).unwrap().is_some());
}

#[tokio::test]
async fn without_card_tools_or_in_shadow_no_picture_is_ever_fetched() {
    let r = hold_rig(None, Some(vec![0xFF, 0xD8, 0xFF, 0]));
    held_dir_for(&r);
    assert!(matches!(go(&r, Some(page_with_picture()), &Prior::default()).await, LadderResult::Held { .. }));
    assert_eq!(r.fake.pics.calls.load(Ordering::SeqCst), 0);
    let mut fake = Fake::new();
    fake.card_ffmpeg = Some(PathBuf::from("no-such-ffmpeg.exe"));
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(60), fake);
    assert!(matches!(go(&r, Some(page_with_picture()), &Prior::default()).await, LadderResult::ShadowFound { .. }));
    assert_eq!(r.fake.pics.calls.load(Ordering::SeqCst), 0, "shadow holds nothing, so there is no card");
}

#[tokio::test]
async fn a_lease_lost_during_the_card_step_writes_no_image_columns() {
    let Some(ffmpeg) = tool("ffmpeg") else {
        eprintln!("WARNING: skipping the lost-lease card test -- bin/ffmpeg.exe missing");
        return;
    };
    let r = hold_rig(Some(ffmpeg.clone()), None);
    let dir = held_dir_for(&r);
    *r.fake.pics.body.lock().unwrap() = Some(preview_and_picture(&ffmpeg, &dir).await);
    // The picture arrives, but the job is no longer this worker's by then.
    let (repo, id) = (r.repo.clone(), r.job.id);
    *r.fake.pics.hook.lock().unwrap() = Some(Box::new(move || {
        assert!(repo.finish(id, OWNER, JobStatus::AwaitingApproval, None, None, None).unwrap());
    }));
    let res = go(&r, Some(page_with_picture()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.pics.calls.load(Ordering::SeqCst), 1);
    let at = r.repo.latest_held_attempt(r.job.id).unwrap().unwrap();
    assert_eq!((at.image_distance, at.image_frame_secs, at.image_host), (None, None, None));
    assert!(!dir.join("_article.jpg").exists() && !dir.join("_frame.jpg").exists(), "a lost job keeps no picture files");
}

// --------------------------------------------------------- through the real engine

fn tool(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

/// A tiny HTTP server for one directory: GET and HEAD, whole files only.
async fn serve(dir: PathBuf) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let dir = dir.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut got = 0;
                loop {
                    let Ok(n) = sock.read(&mut buf[got..]).await else { return };
                    if n == 0 {
                        return;
                    }
                    got += n;
                    if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") || got == buf.len() {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf[..got]).to_string();
                let mut first = head.lines().next().unwrap_or("").split_whitespace();
                let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
                let name = path.trim_start_matches('/').split('?').next().unwrap_or("").to_string();
                let body = if name.contains(['/', '\\']) { None } else { std::fs::read(dir.join(&name)).ok() };
                let reply = match &body {
                    Some(b) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n",
                        b.len()
                    ),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = sock.write_all(reply.as_bytes()).await;
                if method == "GET" {
                    if let Some(b) = body {
                        let _ = sock.write_all(&b).await;
                    }
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

const FAKE_HOST: &str = "cdn.news.example.test";

/// The real ffprobe, with the made-up name mapped to the local server.
struct Rewriting {
    inner: ToolProber,
    addr: SocketAddr,
}
#[async_trait]
impl Prober for Rewriting {
    async fn probe(&self, cand: &Candidate) -> std::result::Result<ProbeInfo, String> {
        let mut c = cand.clone();
        c.url = cand.url.replace(&format!("{FAKE_HOST}:{}", self.addr.port()), &self.addr.to_string());
        self.inner.probe(&c).await
    }
}

struct ToLoopback;
#[async_trait]
impl Resolver for ToLoopback {
    async fn resolve(&self, _host: &str, _port: u16) -> std::result::Result<Vec<IpAddr>, EgressError> {
        Ok(vec!["127.0.0.1".parse().unwrap()])
    }
}

/// The real prober and the real pipeline; only the name is made up.
struct Local {
    engine: Arc<BroadcastEngine>,
    prober: Rewriting,
    addr: SocketAddr,
    fetch: NoFetch,
}

#[async_trait]
impl Env for Local {
    async fn diagnose(&self, _: &str, _: u64) -> Option<PageSnapshot> {
        None
    }
    fn fetcher(&self) -> &dyn Fetcher {
        &self.fetch
    }
    fn prober(&self) -> &dyn Prober {
        &self.prober
    }
    fn resolver(&self) -> &dyn Resolver {
        &ToLoopback
    }
    fn is_ad(&self, _: &str) -> bool {
        false
    }
    fn allow_loopback(&self) -> bool {
        true
    }
    async fn host_is_public(&self, _: &str, _: u16) -> bool {
        true
    }
    async fn download(
        &self,
        mut job: Job,
        owner: &str,
        referer: Option<String>,
        cancel: &CancellationToken,
        mode: DeliveryMode,
    ) -> Result<RunOutcome> {
        job.url = job.url.replace(&format!("{FAKE_HOST}:{}", self.addr.port()), &self.addr.to_string());
        self.engine.process_job_mode(job, owner, referer.as_deref(), Some(BROWSER_UA), None, Some(cancel.clone()), mode).await
    }
}

/// A page whose `__NEXT_DATA__` names a local mp4: found by a shipped recipe,
/// verified by the gate with real ffprobe, downloaded by the real pipeline in
/// Hold mode. The job waits for MCR; nothing reaches the watchfolder.
#[tokio::test]
async fn a_repaired_video_goes_through_the_real_pipeline_and_waits_for_mcr() {
    let (Some(ffmpeg), Some(ffprobe), Some(bmx), Some(ytdlp)) =
        (tool("ffmpeg"), tool("ffprobe"), tool("bmxtranswrap"), tool("yt-dlp"))
    else {
        eprintln!("WARNING: skipping the repair pipeline test -- ffmpeg/ffprobe/bmxtranswrap/yt-dlp not in bin/");
        return;
    };
    let dir = Scratch::new();
    let root = dir.0.clone();
    let www = root.join("www");
    std::fs::create_dir_all(&www).unwrap();
    let args: Vec<String> = [
        "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=6", "-f", "lavfi", "-i",
        "sine=frequency=440:sample_rate=48000:duration=6", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac",
        "-shortest", "-movflags", "+faststart",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([www.join("clip.mp4").to_string_lossy().into_owned()])
    .collect();
    let made = omni_core::process::run(&ffmpeg, &args, omni_core::process::RunOpts::new(Duration::from_secs(120))).await.unwrap();
    assert!(made.success, "could not make the clip");
    let addr = serve(www).await;

    let repo = Repository::new(root.join("omni.db")).unwrap();
    repo.sync_shipped(&recipe::shipped_for_store(), Utc::now()).unwrap();
    std::fs::create_dir_all(root.join("watchfolder")).unwrap();
    let engine = Arc::new(BroadcastEngine::new(
        repo.clone(),
        ytdlp.clone(),
        ffmpeg,
        ffprobe.clone(),
        bmx,
        root.join("temp"),
        root.join("watchfolder"),
    ));
    let env = Local {
        engine,
        prober: Rewriting { inner: ToolProber::new(ffprobe, Some(ytdlp)), addr },
        addr,
        fetch: NoFetch,
    };
    let ctx = RepairCtx { repo: repo.clone(), cfg: cfg(RepairMode::Hold, Duration::from_secs(120)), env: Arc::new(env) };

    let page_url = "https://news.example.test/story/1";
    let id = repo
        .enqueue(&NewJob::new(page_url, "1_ANNA_TEST", "ANNA"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    let job = repo.lease_job(OWNER, 600).unwrap().expect("leased");
    assert_eq!(job.id, id);
    let media = format!("http://{FAKE_HOST}:{}/clip.mp4", addr.port());
    let mut snap = PageSnapshot::new(page_url);
    snap.html = Some(next_data(&media));
    snap.network.push(NetworkEntry { url: format!("http://{FAKE_HOST}:{}/poster.jpg", addr.port()), ..Default::default() });

    let cancel = CancellationToken::new();
    let got = after_failure(&ctx, &job, OWNER, page_url, ErrorCode::VideoUnresolved, &Some(snap), vec![], &cancel).await;
    let Some(LadderResult::Held { recipe_hash }) = got else {
        let ev: Vec<String> = repo.get_job_events(id, 100).unwrap().into_iter().map(|e| e.message).collect();
        panic!("expected Held, got {got:?}; events: {ev:?}")
    };
    assert_eq!(recipe_hash, shipped_hash("next_data_media"));
    // The worker, not the ladder, parks the job.
    assert_eq!(repo.get_job(id).unwrap().unwrap().status, JobStatus::Running);
    assert!(crate::finish_held(&repo, id, OWNER).unwrap());

    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::AwaitingApproval);
    assert_eq!(job.extraction_method, Some(format!("repair:recipe:{}", &recipe_hash[..8])));
    let held = repo.waiting_held(id).unwrap().expect("a waiting held row");
    let held_dir = root.join("temp").join("held").join(id.to_string());
    let names: Vec<String> =
        std::fs::read_dir(&held_dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().any(|n| n.ends_with(".mxf")), "the held MXF: {names:?}");
    assert!(names.iter().any(|n| n.contains("preview")), "the preview: {names:?}");
    assert_eq!(held.sha256, omni_core::held::sha256_file(&held_dir.join("1_ANNA_TEST.mxf")).unwrap());
    assert_eq!(std::fs::read_dir(root.join("watchfolder")).unwrap().count(), 0, "nothing in the watchfolder");
    assert!(!root.join("temp").join("jobs").join(id.to_string()).exists(), "the job's workspace is removed");
    let at = repo.latest_attempt(id).unwrap().unwrap();
    assert_eq!((at.outcome.as_deref(), at.mode.as_str()), (Some("held"), "hold"));
    assert!(at.candidates_json.unwrap().contains("clip.mp4"));
}

// ------------------------------------------- the worker's own path, through run_job

fn engine_in(root: &Path, tools: Option<(PathBuf, PathBuf, PathBuf)>) -> Arc<BroadcastEngine> {
    let (ffmpeg, ffprobe, bmx) = tools.unwrap_or_else(|| (root.join("no-ffmpeg.exe"), root.join("no-ffprobe.exe"), root.join("no-bmx.exe")));
    std::fs::create_dir_all(root.join("watchfolder")).unwrap();
    Arc::new(BroadcastEngine::new(
        Repository::new(root.join("omni.db")).unwrap(),
        root.join("no-yt-dlp.exe"),
        ffmpeg,
        ffprobe,
        bmx,
        root.join("temp"),
        root.join("watchfolder"),
    ))
}

async fn work(repo: &Repository, engine: &Arc<BroadcastEngine>, root: &Path, job: Job, mode: RepairMode) {
    let ctx = RepairCtx { repo: repo.clone(), cfg: cfg(mode, Duration::from_secs(5)), env: Arc::new(PanicEnv) };
    let health = omni_core::health::HealthState::new();
    let canary = Arc::new(crate::canary::CanaryEnv::new(repo.clone(), health.clone(), root.join("bin"), "stable".into()));
    crate::run_job(repo, engine, OWNER, job, CancellationToken::new(), &root.join("evidence"), &ctx, &health, &canary)
        .await
        .unwrap();
}

/// A failure that is not an unresolved video on a web page ends exactly as
/// before, with the ladder (a panicking env) never reached, in every mode.
#[tokio::test]
async fn guarantee_a_non_eligible_failure_through_run_job_never_calls_the_ladder() {
    for mode in [RepairMode::Off, RepairMode::Shadow, RepairMode::Hold] {
        let dir = Scratch::new();
        let root = dir.0.clone();
        let engine = engine_in(&root, None);
        let repo = Repository::new(root.join("omni.db")).unwrap();
        let mut new = NewJob::new("attachment://m1@example.gr/1", "1_ANNA_TEST", "ANNA");
        new.extraction_method = Some("attachment".into());
        let id = repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
        let job = repo.lease_job(OWNER, 600).unwrap().expect("leased");
        assert_eq!(job.id, id);
        work(&repo, &engine, &root, job, mode).await;
        let done = repo.get_job(id).unwrap().unwrap();
        assert_ne!(done.status, JobStatus::AwaitingApproval, "{mode:?}");
        assert!(done.status.is_terminal() || done.status == JobStatus::ManualDownload, "{mode:?}: {:?}", done.status);
        assert!(repo.latest_attempt(id).unwrap().is_none(), "{mode:?}: the ladder left a record");
    }
}

/// A job whose normal path succeeds never reaches the ladder: it is delivered,
/// COMPLETED, and the panicking env is never touched.
#[tokio::test]
async fn guarantee_a_successful_job_never_calls_the_ladder() {
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (tool("ffmpeg"), tool("ffprobe"), tool("bmxtranswrap")) else {
        eprintln!("WARNING: skipping the successful-job guarantee -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return;
    };
    let dir = Scratch::new();
    let root = dir.0.clone();
    let source = root.join("attachments").join("1").join("source.mp4");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let args: Vec<String> = [
        "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=2", "-f", "lavfi", "-i",
        "sine=frequency=1000:sample_rate=48000:duration=2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([source.to_string_lossy().into_owned()])
    .collect();
    assert!(omni_core::process::run(&ffmpeg, &args, omni_core::process::RunOpts::new(Duration::from_secs(120))).await.unwrap().success);
    let engine = engine_in(&root, Some((ffmpeg, ffprobe, bmx)));
    let repo = Repository::new(root.join("omni.db")).unwrap();
    let mut new = NewJob::new("attachment://m1@example.gr/1", "1_ANNA_TEST", "ANNA");
    new.extraction_method = Some("attachment".into());
    new.status = JobStatus::ManualDownload;
    let id = repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    assert!(repo.attach_source(id, &source.to_string_lossy()).unwrap());
    let job = repo.lease_job(OWNER, 600).unwrap().expect("released job is leasable");
    work(&repo, &engine, &root, job, RepairMode::Hold).await;
    let done = repo.get_job(id).unwrap().unwrap();
    assert_eq!(done.status, JobStatus::Completed, "{:?}", done.error_message);
    assert!(repo.latest_attempt(id).unwrap().is_none());
    assert_eq!(std::fs::read_dir(root.join("watchfolder")).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".mxf")).count(), 1);
}

// ---- P10.11c: recipes with live actions, one live page load per ladder run ----

const LIVE_A: &str = "https://cdn.news.example.gr/live/a.m3u8";
const LIVE_B: &str = "https://cdn.news.example.gr/live/b.m3u8";
/// A video host the passive page never showed: observed only after the click.
const LIVE_OTHER: &str = "https://video.player-cdn.example.net/live/c.m3u8";

/// Store a recipe of `scope` ("site" on the page's domain, or "generic");
/// steps `[click css, net re, media]`. Returns its row id.
fn add_live_recipe(r: &Rig, scope: &str, click: &str, url_re: &str) -> i64 {
    use omni_core::recipe_score::{InsertOutcome, NewRecipe, Origin, ScopeKind};
    let json = format!(
        r#"{{"dsl_version":1,"steps":[{{"op":"click","css":"{click}"}},{{"op":"net","url_re":"{url_re}"}},{{"op":"media","from":1,"referer":"page"}}]}}"#
    );
    let rec = Recipe::from_json(&json).unwrap();
    let key = if scope == "site" { DOMAIN } else { "live-test" };
    let new = NewRecipe {
        scope_kind: if scope == "site" { ScopeKind::Site } else { ScopeKind::Generic },
        scope_key: key.to_string(),
        family: None,
        recipe_json: rec.canonical_json(),
        hash: recipe::scope_hash(scope, key, &rec),
        steps_hash: recipe::steps_hash(&rec),
        dsl_version: rec.dsl_version,
        origin: Origin::Admin,
    };
    match r.repo.insert_recipe(&new, Utc::now()).unwrap() {
        InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) => id,
        other => panic!("recipe not stored: {other:?}"),
    }
}

fn live_page(urls: &[&str]) -> PageSnapshot {
    let mut s = page("<html><body><button id=\"load\">play</button></body></html>");
    for u in urls {
        s.network.push(NetworkEntry {
            url: (*u).to_string(),
            method: Some("GET".into()),
            status: Some(200),
            mime: None,
            resource_type: None,
            frame_id: None,
        });
    }
    s
}

fn set_live(f: &Fake, snap: PageSnapshot, complete: LiveRun, outcomes: &[&str]) {
    *f.live_result.lock().unwrap() = Some((snap, complete, outcomes.iter().map(|s| s.to_string()).collect()));
}

/// Whether the recipe has an event row on the page's domain.
fn has_event(r: &Rig, id: i64) -> bool {
    r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap().iter().find(|x| x.id == id).unwrap().site_state.is_some()
}

fn blank() -> PageSnapshot {
    page("<html><body><p>no video yet</p></body></html>")
}

#[tokio::test]
async fn three_live_recipes_cost_one_live_load_and_those_sharing_its_actions_are_all_read() {
    let fake = Fake::new();
    let live_o = "https://cdn.news.example.gr/live/o.m3u8";
    set_live(&fake, live_page(&[LIVE_A, LIVE_B, live_o]), LiveRun::Complete, &["clicked"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
    // (id, click, the address its regex finds): two share the actions, one differs.
    let made = [
        (add_live_recipe(&r, "site", "#play", "a[.]m3u8"), "#play", LIVE_A),
        (add_live_recipe(&r, "site", "#play", "b[.]m3u8"), "#play", LIVE_B),
        (add_live_recipe(&r, "site", "#load", "o[.]m3u8"), "#load", live_o),
    ];
    // Which one the ladder takes first is the store's ranking, not this test's business.
    let order: Vec<i64> = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap().iter().map(|x| x.id).collect();
    let first = made.iter().min_by_key(|m| order.iter().position(|i| *i == m.0).unwrap()).unwrap();
    let res = go(&r, Some(blank()), &Prior::default()).await;
    let LadderResult::ShadowFound { verified } = res else { panic!("expected a find, got {res:?}") };
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 1, "one live load, three live recipes");
    assert_eq!(*r.fake.live_seen.lock().unwrap(), vec![vec![LiveAction::Click(first.1.into())]]);
    let mut urls: Vec<String> = verified.iter().map(|v| v.candidate.url.clone()).collect();
    urls.sort();
    let mut want: Vec<String> = made.iter().filter(|m| m.1 == first.1).map(|m| m.2.to_string()).collect();
    want.sort();
    assert_eq!(want.len(), 2, "the test needs the shared pair to be first");
    assert_eq!(urls, want, "every recipe sharing the first one's actions was read, no other");
    for m in &made {
        assert!(!has_event(&r, m.0), "a read recipe found its video; one with other actions is skipped without an event");
    }
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!(at.rung.as_deref(), Some("recipes_live:clicked"));
}

#[tokio::test]
async fn a_live_candidate_goes_through_the_gate_with_the_passive_ones() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[LIVE_A]), LiveRun::Complete, &["clicked"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
    add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let res = go(&r, Some(page(&next_data(B))), &Prior::default()).await;
    let LadderResult::ShadowFound { verified } = res else { panic!("expected a find, got {res:?}") };
    let urls: Vec<&str> = verified.iter().map(|v| v.candidate.url.as_str()).collect();
    assert!(urls.contains(&B) && urls.contains(&LIVE_A), "{urls:?}");
    assert_eq!(r.fake.probes(), 2, "one gate run for both");
    assert!(verified.iter().find(|v| v.candidate.url == LIVE_A).unwrap().candidate.origin.starts_with("recipe:"));
}

#[tokio::test]
async fn hosts_seen_only_after_the_click_count_as_observed_for_the_gate() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[LIVE_OTHER]), LiveRun::Complete, &["clicked"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
    add_live_recipe(&r, "site", "#load", "c[.]m3u8");
    let res = go(&r, Some(blank()), &Prior::default()).await;
    let LadderResult::ShadowFound { verified } = res else { panic!("expected a find, got {res:?}") };
    assert_eq!(verified[0].candidate.url, LIVE_OTHER);
}

#[tokio::test]
async fn an_incomplete_live_run_gives_no_candidates_and_no_events() {
    let fake = Fake::new();
    // The click found nothing; the network shows a video anyway (must not be used).
    set_live(&fake, live_page(&[LIVE_A]), LiveRun::Inconclusive, &["timeout"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
    let id = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let res = go(&r, Some(blank()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.probes(), 0);
    assert!(!has_event(&r, id), "an incomplete run says nothing about the recipe");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!(at.rung.as_deref(), Some("recipes_live:timeout"));
}

#[tokio::test]
async fn a_complete_live_run_that_finds_nothing_is_a_miss_for_the_recipe() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[]), LiveRun::Complete, &["clicked"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
    let id = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let res = go(&r, Some(blank()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert!(has_event(&r, id), "complete run, no value: NoCandidate");
}

#[tokio::test]
async fn a_live_candidate_the_gate_rejects_conclusively_is_a_miss_for_the_recipe() {
    let fake = Fake::new();
    fake.probe.lock().unwrap().insert(LIVE_A.to_string(), Err("ffprobe could not read the address".to_string()));
    set_live(&fake, live_page(&[LIVE_A]), LiveRun::Complete, &["clicked"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
    let id = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let res = go(&r, Some(blank()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.probes(), 1);
    assert!(has_event(&r, id));
}

#[tokio::test]
async fn with_less_than_thirty_seconds_left_there_is_no_live_load() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[LIVE_A]), LiveRun::Complete, &["clicked"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(25), fake);
    let id = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let res = go(&r, Some(blank()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 0);
    assert!(!has_event(&r, id), "a recipe that was not tried has no event");
}

#[tokio::test]
async fn cancel_during_the_live_load_is_lost_and_writes_nothing() {
    let mut fake = Fake::new();
    fake.live_hangs = true;
    let r = rig_with(RepairMode::Hold, Duration::from_secs(120), fake);
    let id = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let res = run(&r.ctx, &r.job, OWNER, PAGE, Some(blank()), &Prior::default(), &cancel).await;
    assert!(matches!(res, LadderResult::Lost), "{res:?}");
    assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 1);
    assert!(!has_event(&r, id));
    assert!(r.fake.downloads().is_empty());
    assert!(events(&r).iter().all(|m| !m.contains("επισκευή")), "{:?}", events(&r));
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!(at.outcome.as_deref(), Some("lost"));
}

#[tokio::test]
async fn without_live_recipes_the_ladder_never_asks_for_a_live_load() {
    let mut fake = Fake::new();
    fake.live_panics = true;
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
    let res = go(&r, Some(page(&next_data(A))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }), "{res:?}");
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 0);
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    assert_eq!(at.rung.as_deref(), Some("recipes"));
}

#[tokio::test]
async fn a_live_load_that_overruns_never_eats_the_gates_time() {
    // The live run sleeps far past its allowance; a passive candidate exists.
    let mut fake = Fake::new();
    fake.live_hangs = true;
    fake.live_limits = Some((1, Duration::from_millis(500)));
    fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let r = rig_with(RepairMode::Hold, Duration::from_secs(120), fake);
    let id = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let t = Instant::now();
    let res = go(&r, Some(page(&next_data(B))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::Held { .. }), "{res:?}");
    assert_eq!(r.fake.downloads().iter().map(|d| d.0.clone()).collect::<Vec<_>>(), vec![B.to_string()]);
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 1);
    assert!(t.elapsed() < Duration::from_secs(10), "the wait ended at the ladder deadline: {:?}", t.elapsed());
    assert!(!has_event(&r, id), "an overrun says nothing about the recipe");
}

#[tokio::test]
async fn too_little_budget_for_the_gate_reserve_means_no_live_load() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[LIVE_A]), LiveRun::Complete, &["clicked"]);
    // 40 s is over the old 30 s rule, under 30 s + the gate's 20 s.
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(40), fake);
    add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let res = go(&r, Some(page(&next_data(B))), &Prior::default()).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }), "{res:?}");
    assert_eq!(r.fake.live_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_click_target_missing_on_a_clean_page_is_a_miss_for_every_recipe_with_those_actions() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[LIVE_A, LIVE_B]), LiveRun::Missing, &["missing"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
    let a = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    let b = add_live_recipe(&r, "site", "#load", "b[.]m3u8");
    let res = go(&r, Some(blank()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.probes(), 0, "a missing target gives no candidates, even if the network shows a video");
    assert!(has_event(&r, a) && has_event(&r, b), "NoCandidate for each recipe sharing the actions");
    assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().rung.as_deref(), Some("recipes_live:missing"));
}

#[tokio::test]
async fn an_inconclusive_live_run_gives_no_events_whatever_its_outcome() {
    for outcome in ["timeout", "error", "refused", "skipped"] {
        let fake = Fake::new();
        set_live(&fake, live_page(&[]), LiveRun::Inconclusive, &[outcome]);
        let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), fake);
        let id = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
        go(&r, Some(blank()), &Prior::default()).await;
        assert!(!has_event(&r, id), "{outcome}");
    }
}

#[tokio::test]
async fn no_live_note_when_the_browser_never_ran_the_live_load() {
    // The fake returns None, as RealEnv does for a launch failure.
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(120), Fake::new());
    let id = add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    go(&r, Some(blank()), &Prior::default()).await;
    assert_eq!(r.repo.latest_attempt(r.job.id).unwrap().unwrap().rung.as_deref(), Some("recipes"));
    assert!(!has_event(&r, id));
}

#[test]
fn a_run_without_action_outcomes_is_no_live_load() {
    use omni_browser::evidence::Diagnostic;
    let acts = [LiveAction::Click("#load".into())];
    // launch failure: the default diagnostic
    assert!(live_result(blank(), &Diagnostic::default(), &acts).is_none());
    let ran = Diagnostic { live_actions: vec!["clicked".into()], complete: true, live_page_ok: true, ..Default::default() };
    let (_, run, outcomes) = live_result(blank(), &ran, &acts).unwrap();
    assert_eq!((run, outcomes), (LiveRun::Complete, vec!["clicked".to_string()]));
}

#[tokio::test]
async fn the_live_seconds_leave_the_gate_its_reserve() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[LIVE_A]), LiveRun::Complete, &["clicked"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(55), fake);
    add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    go(&r, Some(page(&next_data(B))), &Prior::default()).await;
    let secs = r.fake.live_secs_seen.lock().unwrap().clone();
    assert_eq!(secs.len(), 1, "55 s left is enough for a live load");
    // left is at most 55 s: the load may use at most 35 s, never the gate's 20 s.
    assert!(secs[0] <= 35 && secs[0] >= 30, "{secs:?}");
}

#[tokio::test]
async fn under_fifty_seconds_left_no_live_load_is_requested() {
    let fake = Fake::new();
    set_live(&fake, live_page(&[LIVE_A]), LiveRun::Complete, &["clicked"]);
    let r = rig_with(RepairMode::Shadow, Duration::from_secs(49), fake);
    add_live_recipe(&r, "site", "#load", "a[.]m3u8");
    go(&r, Some(page(&next_data(B))), &Prior::default()).await;
    assert!(r.fake.live_secs_seen.lock().unwrap().is_empty());
}

#[path = "repair_llm_tests.rs"]
mod llm;

// ---- P10.13: learning from MCR, «Επόμενη επιλογή» ----

/// MCR called `url` a wrong video on `DOMAIN`: another job of the store held it and was rejected as wrong.
fn mcr_calls_wrong(repo: &Repository, n: u32, url: &str) {
    let id = repo
        .enqueue(&NewJob::new(format!("https://news.example.gr/other-{n}"), format!("{n}_PETROU_X"), "PETROU"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    assert!(repo.lease_job_by_id(id, "desk-test", 600).unwrap());
    let at = repo.start_attempt(id, "hold", Utc::now()).unwrap();
    let end = AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        domain: Some(DOMAIN.into()),
        held_url: Some(url.to_string()),
        candidates_json: Some(serde_json::json!([{"url": url}]).to_string()),
        ..Default::default()
    };
    repo.finish_attempt(at, &end, Utc::now()).unwrap();
    assert!(repo.hold_output(id, "desk-test", "temp/held/x/clip.mxf", 3, &"ab".repeat(32), Some(9.0), Utc::now() + chrono::Duration::hours(24)).unwrap());
    assert!(repo.finish(id, "desk-test", JobStatus::AwaitingApproval, None, None, None).unwrap());
    assert!(repo.reject_held(id, "wrong_video", "no", "desk").unwrap());
}

#[tokio::test]
async fn a_skipped_address_is_dropped_before_the_gate_with_no_recipe_event() {
    let skip = omni_core::urlnorm::skip_pattern(A).unwrap();
    // Control: without the skip the same page is found and probed.
    let r = rig(RepairMode::Shadow);
    assert!(matches!(go(&r, Some(page(&next_data(A))), &Prior::default()).await, LadderResult::ShadowFound { .. }));
    assert!(r.fake.probes() > 0);
    // The query is not part of the pattern: the skip holds for a changed token.
    let r = rig(RepairMode::Shadow);
    let p = Prior { skips: vec![skip], ..Default::default() };
    let res = go(&r, Some(page(&next_data(&format!("{A}?token=new")))), &p).await;
    assert!(matches!(res, LadderResult::NothingVerified), "{res:?}");
    assert_eq!(r.fake.probes(), 0, "dropped before the gate");
    let rows = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap();
    let hit = rows.iter().find(|x| x.hash == shipped_hash("next_data_media")).unwrap();
    assert!(hit.site_state.is_none(), "a skip is MCR's doing, not a miss of the recipe");
}

#[tokio::test]
async fn the_ladder_reads_the_skip_list_of_the_site_from_the_store() {
    let r = rig(RepairMode::Hold);
    mcr_calls_wrong(&r.repo, 1, &format!("{A}?sig=1"));
    r.fake.script.lock().unwrap().push_back(Ok(RunOutcome::Held));
    let got = after_failure(&r.ctx, &r.job, OWNER, PAGE, ErrorCode::VideoUnresolved, &Some(page(&next_data(A))), vec![], &CancellationToken::new()).await;
    assert!(matches!(got, Some(LadderResult::NothingVerified)), "{got:?}");
    assert_eq!(r.fake.probes(), 0);
    assert!(r.fake.downloads().is_empty(), "the wrong video is not downloaded again");
}

#[tokio::test]
async fn the_attempt_names_the_recipe_behind_every_verified_candidate() {
    let r = rig(RepairMode::Shadow);
    let res = go(&r, Some(three()), &Prior::default()).await;
    assert!(matches!(res, LadderResult::ShadowFound { .. }), "{res:?}");
    let at = r.repo.latest_attempt(r.job.id).unwrap().unwrap();
    let list: serde_json::Value = serde_json::from_str(at.candidates_json.as_deref().unwrap()).unwrap();
    let rows = r.repo.recipes_for_site(DOMAIN, None, Utc::now()).unwrap();
    let id_of = |hash: &str| rows.iter().find(|x| x.hash == hash).unwrap().id;
    let (first, second) = (shipped_hash("og_video"), shipped_hash("next_data_media"));
    assert_eq!(list[0]["url"], A);
    assert_eq!((list[0]["recipe_id"].as_i64(), list[0]["recipe_hash"].as_str()), (Some(id_of(&first)), Some(first.as_str())));
    assert_eq!((list[1]["recipe_id"].as_i64(), list[1]["recipe_hash"].as_str()), (Some(id_of(&second)), Some(second.as_str())));
    assert!(list[1]["origin"].as_str().unwrap().starts_with("recipe:"), "the existing fields stay");
}

/// The world of the next-candidate path: it may only download (and resolve the
/// host just before). Anything else the ladder uses panics the test.
struct NextEnv {
    downloads: Mutex<Vec<(String, Option<String>, DeliveryMode)>>,
    script: Mutex<VecDeque<std::result::Result<RunOutcome, ErrorCode>>>,
}

impl NextEnv {
    fn new(script: Vec<std::result::Result<RunOutcome, ErrorCode>>) -> Arc<Self> {
        Arc::new(Self { downloads: Mutex::new(Vec::new()), script: Mutex::new(script.into()) })
    }
}

#[async_trait]
impl Env for NextEnv {
    async fn diagnose(&self, _: &str, _: u64) -> Option<PageSnapshot> {
        panic!("the next path looked at the page")
    }
    async fn live_snapshot(&self, _: &str, _: &[LiveAction], _: u64) -> Option<(PageSnapshot, LiveRun, Vec<String>)> {
        panic!("the next path asked for a live page load")
    }
    fn model(&self) -> Option<&dyn RepairModel> {
        panic!("the next path asked for the model")
    }
    fn fetcher(&self) -> &dyn Fetcher {
        panic!("the next path ran a recipe")
    }
    fn prober(&self) -> &dyn Prober {
        panic!("the next path ran the gate")
    }
    fn resolver(&self) -> &dyn Resolver {
        panic!("the next path ran the gate")
    }
    fn is_ad(&self, _: &str) -> bool {
        panic!("the next path ran the gate")
    }
    async fn host_is_public(&self, _: &str, _: u16) -> bool {
        true
    }
    async fn download(&self, job: Job, _: &str, referer: Option<String>, _: &CancellationToken, mode: DeliveryMode) -> Result<RunOutcome> {
        self.downloads.lock().unwrap().push((job.url.clone(), referer, mode));
        match self.script.lock().unwrap().pop_front() {
            Some(Ok(o)) => Ok(o),
            Some(Err(code)) => Err(anyhow::anyhow!("scripted failure").context(code.as_str())),
            None => panic!("a download nobody scripted"),
        }
    }
}

struct NextRig {
    _dir: Scratch,
    root: PathBuf,
    repo: Repository,
    engine: Arc<BroadcastEngine>,
    job_id: i64,
}

/// A job that held candidate #1 of [A, B, C] (recipe-made, with the recipe's id and hash on
/// each entry) and that MCR answered with «Επόμενη επιλογή»: PENDING, a `next` attempt on B.
fn next_rig() -> NextRig {
    let dir = Scratch::new();
    let root = dir.0.clone();
    let engine = engine_in(&root, None);
    let repo = Repository::new(root.join("omni.db")).unwrap();
    let job = leased_job(&repo);
    let entry = |url: &str, id: i64| {
        serde_json::json!({
            "url": url, "referer": PAGE, "origin": format!("recipe:{:08}", id), "duration_secs": 61.0,
            "recipe_id": id, "recipe_hash": format!("{id:064}"), "checks": [],
        })
    };
    let at = repo.start_attempt(job.id, "hold", Utc::now()).unwrap();
    let end = AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        domain: Some(DOMAIN.into()),
        candidates_json: Some(serde_json::json!([entry(A, 1), entry(B, 2), entry(C, 3)]).to_string()),
        held_url: Some(A.to_string()),
        next_index: 0,
        expected_secs: Some(60.0),
        ..Default::default()
    };
    repo.finish_attempt(at, &end, Utc::now()).unwrap();
    assert!(repo.hold_output(job.id, OWNER, "temp/held/x/clip.mxf", 3, &"ab".repeat(32), Some(61.0), Utc::now() + chrono::Duration::hours(24)).unwrap());
    assert!(repo.finish(job.id, OWNER, JobStatus::AwaitingApproval, None, None, None).unwrap());
    assert_eq!(repo.next_candidate(job.id, &"ab".repeat(32), "desk", Utc::now()).unwrap(), Some(1));
    NextRig { _dir: dir, root, repo, engine, job_id: job.id }
}

impl NextRig {
    fn lease(&self) -> Job {
        self.repo.lease_job(OWNER, 600).unwrap().expect("the job is queued again")
    }

    async fn run(&self, env: Arc<NextEnv>, job: Job) {
        let mut c = cfg(RepairMode::Hold, Duration::from_secs(5));
        c.llm = LlmCfg { enabled: true, model: Some("m".into()), ..LlmCfg::default() };
        let ctx = RepairCtx { repo: self.repo.clone(), cfg: c, env };
        let health = omni_core::health::HealthState::new();
        let canary = Arc::new(crate::canary::CanaryEnv::new(self.repo.clone(), health.clone(), self.root.join("bin"), "stable".into()));
        crate::run_job(&self.repo, &self.engine, OWNER, job, CancellationToken::new(), &self.root.join("evidence"), &ctx, &health, &canary)
            .await
            .unwrap();
    }
}

/// «Επόμενη επιλογή» through the worker: candidate #2 is downloaded held with the
/// hold the ladder uses, and nothing else of the ladder (page look, recipes, model,
/// the normal download -- the engine here has no tools at all) is reached.
#[tokio::test]
async fn the_next_choice_holds_candidate_two_and_never_reruns_the_rungs() {
    let n = next_rig();
    let env = NextEnv::new(vec![Ok(RunOutcome::Held)]);
    n.run(env.clone(), n.lease()).await;

    let d = env.downloads.lock().unwrap().clone();
    assert_eq!(d.len(), 1, "one download, of the next candidate only");
    assert_eq!((d[0].0.as_str(), d[0].1.as_deref()), (B, Some(PAGE)));
    assert_eq!(d[0].2, DeliveryMode::Hold { hold_hours: 24, held_max_bytes: 20 << 30 }, "held again, for MCR");
    let job = n.repo.get_job(n.job_id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::AwaitingApproval, "parked by the worker: {:?}", job.error_message);
    assert_eq!(job.extraction_method, Some(format!("repair:recipe:{:08}", 2)), "named after the recipe behind #2");
    let at = n.repo.latest_attempt(n.job_id).unwrap().unwrap();
    assert_eq!((at.rung.as_deref(), at.outcome.as_deref()), (Some("next"), Some("held")));
    assert_eq!((at.held_url.as_deref(), at.next_index, at.recipe_id), (Some(B), 1, Some(2)));
    assert_eq!(at.recipe_hash.as_deref(), Some(format!("{:064}", 2).as_str()));
    assert_eq!(at.expected_secs, Some(60.0), "the page's stated length is carried over");
    assert!(at.finished_at.is_some());
    // It is the attempt the card describes now, and C is still offered.
    assert_eq!(n.repo.latest_held_attempt(n.job_id).unwrap().unwrap().id, at.id);
    assert_eq!(n.repo.next_left(n.job_id, Utc::now()).unwrap(), 1);
    assert_eq!(n.repo.rejected_held_urls(n.job_id).unwrap(), vec![A.to_string()]);
}

#[tokio::test]
async fn a_failed_next_download_ends_in_review_with_the_greek_message_and_tries_nothing_else() {
    let n = next_rig();
    let env = NextEnv::new(vec![Err(ErrorCode::Http403), Ok(RunOutcome::Held)]);
    n.run(env.clone(), n.lease()).await;
    assert_eq!(env.downloads.lock().unwrap().len(), 1, "no further candidate by itself, though C is verified");
    let job = n.repo.get_job(n.job_id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::RequiresReview);
    assert_eq!(job.error_code.as_deref(), Some("VIDEO_UNRESOLVED"));
    assert_eq!(job.error_message.as_deref(), Some("Η επόμενη επιλογή δεν κατέβηκε"));
    assert_eq!(job.extraction_method, None, "the repair mark is taken back");
    let at = n.repo.latest_attempt(n.job_id).unwrap().unwrap();
    assert_eq!((at.rung.as_deref(), at.outcome.as_deref()), (Some("next"), Some("next_failed")));
    assert!(n.repo.get_held(n.job_id).unwrap().is_some_and(|h| h.decision.as_deref() == Some("rejected")), "nothing is held");
}

#[tokio::test]
async fn the_next_path_checks_the_skip_list_and_rejections_again_before_downloading() {
    // B was called a wrong video (another job) after MCR chose it.
    let n = next_rig();
    mcr_calls_wrong(&n.repo, 7, &format!("{B}?sig=zz"));
    let env = NextEnv::new(vec![Ok(RunOutcome::Held)]);
    n.run(env.clone(), n.lease()).await;
    assert!(env.downloads.lock().unwrap().is_empty(), "a skipped address is never downloaded");
    let job = n.repo.get_job(n.job_id).unwrap().unwrap();
    assert_eq!((job.status, job.error_message.as_deref()), (JobStatus::RequiresReview, Some("Η επόμενη επιλογή δεν κατέβηκε")));
    assert_eq!(n.repo.latest_attempt(n.job_id).unwrap().unwrap().outcome.as_deref(), Some("next_skipped"));

    // The rejected-URL half: MCR rejected the address for this very job (not as «wrong»).
    let dir = Scratch::new();
    let repo = Repository::new(dir.0.join("omni.db")).unwrap();
    let job = leased_job(&repo);
    let at = repo.start_attempt(job.id, "hold", Utc::now()).unwrap();
    let end = AttemptEnd { rung: Some("recipes".into()), outcome: Some("held".into()), domain: Some(DOMAIN.into()), held_url: Some(B.into()), ..Default::default() };
    repo.finish_attempt(at, &end, Utc::now()).unwrap();
    assert!(repo.hold_output(job.id, OWNER, "temp/held/x/clip.mxf", 3, &"ab".repeat(32), Some(9.0), Utc::now() + chrono::Duration::hours(24)).unwrap());
    assert!(repo.finish(job.id, OWNER, JobStatus::AwaitingApproval, None, None, None).unwrap());
    assert_eq!(next_barred(&repo, job.id, DOMAIN, B), Some(false), "still undecided");
    assert!(repo.reject_held(job.id, "not_usable", "no", "desk").unwrap());
    assert_eq!(next_barred(&repo, job.id, DOMAIN, B), Some(true), "rejected for this job");
    assert_eq!(next_barred(&repo, job.id, DOMAIN, &format!("{B}#t=3")), Some(true), "on the normalized address");
    assert_eq!(next_barred(&repo, job.id, DOMAIN, C), Some(false));
}

#[tokio::test]
async fn the_next_path_writes_nothing_once_the_lease_is_gone() {
    let n = next_rig();
    let job = n.lease();
    let ctx = RepairCtx { repo: n.repo.clone(), cfg: cfg(RepairMode::Hold, Duration::from_secs(5)), env: NextEnv::new(vec![Ok(RunOutcome::Held)]) };
    let attempt = open_next(&n.repo, n.job_id).unwrap().expect("an open next attempt");
    let got = run_next(&ctx, &job, "someone-else", &n.root.join("evidence"), &attempt, &CancellationToken::new()).await;
    assert_eq!(got, NextResult::Lost);
    assert!(open_next(&n.repo, n.job_id).unwrap().is_some(), "the attempt stays open: a new lease does it again");
    assert_eq!(n.repo.get_job(n.job_id).unwrap().unwrap().status, JobStatus::Running);
}

/// A plain failed job with an older ladder attempt is not a next run.
#[tokio::test]
async fn only_an_unfinished_next_attempt_is_a_next_run() {
    let r = rig(RepairMode::Hold);
    assert!(open_next(&r.repo, r.job.id).unwrap().is_none(), "no attempt");
    let at = r.repo.start_attempt(r.job.id, "hold", Utc::now()).unwrap();
    assert!(open_next(&r.repo, r.job.id).unwrap().is_none(), "an open attempt that is not a next one");
    r.repo.finish_attempt(at, &AttemptEnd { rung: Some("next".into()), outcome: Some("held".into()), ..Default::default() }, Utc::now()).unwrap();
    assert!(open_next(&r.repo, r.job.id).unwrap().is_none(), "a finished next attempt");
}

/// With repair off or in shadow a pending next choice downloads nothing: review, with the Greek reason.
#[tokio::test]
async fn a_pending_next_choice_is_not_run_unless_repair_mode_is_hold() {
    for mode in [RepairMode::Off, RepairMode::Shadow] {
        let n = next_rig();
        let env = NextEnv::new(vec![Ok(RunOutcome::Held)]);
        let ctx = RepairCtx { repo: n.repo.clone(), cfg: cfg(mode, Duration::from_secs(5)), env: env.clone() };
        let health = omni_core::health::HealthState::new();
        let canary = Arc::new(crate::canary::CanaryEnv::new(n.repo.clone(), health.clone(), n.root.join("bin"), "stable".into()));
        crate::run_job(&n.repo, &n.engine, OWNER, n.lease(), CancellationToken::new(), &n.root.join("evidence"), &ctx, &health, &canary)
            .await
            .unwrap();
        assert!(env.downloads.lock().unwrap().is_empty(), "{mode:?}: nothing is downloaded");
        let job = n.repo.get_job(n.job_id).unwrap().unwrap();
        assert_eq!((job.status, job.error_message.as_deref()), (JobStatus::RequiresReview, Some("Η αυτόματη επισκευή είναι απενεργοποιημένη")), "{mode:?}");
        assert_eq!(n.repo.latest_attempt(n.job_id).unwrap().unwrap().outcome.as_deref(), Some("repair_off"));
    }
}
