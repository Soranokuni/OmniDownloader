//! The benchmark runner with fake stages: what a run may and may not touch, how
//! many targets run at once, and that no stream address is kept. No tools, no network.

use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

/// A scratch install root that removes itself.
struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("omni-bench-test-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn paths(&self) -> AppPaths {
        AppPaths::with_root(&self.0, "config.json")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SECRET_STREAM: &str = "https://cdn.example.net/live/secret-path/master.m3u8?token=tok-8f31";

/// Stages that answer from a script and count what is going on.
#[derive(Default)]
struct Fake {
    /// Targets whose extraction fails.
    no_video: Vec<i64>,
    /// Targets whose download fails.
    no_download: Vec<i64>,
    /// How long extraction takes.
    extract_delay_ms: u64,
    live: AtomicUsize,
    peak: AtomicUsize,
    extracted: AtomicUsize,
    downloaded: AtomicUsize,
    dirs_seen: Mutex<Vec<PathBuf>>,
    /// The duration every source probes as; 0 means a short clip.
    source_secs: f64,
    /// Targets whose source probes as longer than ten minutes.
    long: Vec<i64>,
    /// A length the extraction already knows (yt-dlp's probe), if any.
    known_secs: Option<f64>,
    transcodes: AtomicUsize,
    /// Run after each download, with the target's number.
    after_download: Mutex<Option<Box<dyn Fn(i64) + Send + Sync>>>,
    /// Run while a target is being extracted, with the target's number.
    during_extract: Mutex<Option<Box<dyn Fn(i64) + Send + Sync>>>,
    /// A gate to read the number of passes from while a download runs.
    watch_gate: Mutex<Option<UpdateGate>>,
    gate_active_at_download: AtomicUsize,
    dir_existed_at_verify: AtomicBool,
}

#[async_trait]
impl BenchStages for Fake {
    async fn extract(&self, target: &TargetRow) -> StageResult<Extracted> {
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(self.extract_delay_ms)).await;
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.extracted.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = self.during_extract.lock().unwrap().as_ref() {
            hook(target.idx);
        }
        if self.no_video.contains(&target.idx) {
            return Err("NO_STREAM_FOUND".into());
        }
        Ok(Extracted {
            method: "sniffer".into(),
            candidates: 2,
            duration_secs: self.known_secs,
            stream: SECRET_STREAM.into(),
            referer: Some("https://news.example/story".into()),
            user_agent: None,
            cookies: Some("sid=cookie-value-1".into()),
        })
    }

    async fn download(&self, target: &TargetRow, _found: &Extracted, dir: &Path) -> StageResult<PathBuf> {
        self.downloaded.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.watch_gate.lock().unwrap().as_ref() {
            self.gate_active_at_download.store(gate.active(), Ordering::SeqCst);
        }
        self.dirs_seen.lock().unwrap().push(dir.to_path_buf());
        if self.no_download.contains(&target.idx) {
            // Failed after writing a partial file: it must go with the directory.
            std::fs::write(dir.join("source.part"), b"partial").unwrap();
            return Err("HTTP_403".into());
        }
        let file = dir.join("source.mp4");
        std::fs::write(&file, b"video").unwrap();
        if let Some(hook) = self.after_download.lock().unwrap().as_ref() {
            hook(target.idx);
        }
        Ok(file)
    }

    async fn source_secs(&self, target: &TargetRow, _source: &Path) -> StageResult<f64> {
        if self.long.contains(&target.idx) {
            return Ok(601.0);
        }
        Ok(if self.source_secs > 0.0 { self.source_secs } else { 12.0 })
    }

    async fn transcode(&self, _target: &TargetRow, source: &Path, _duration: f64, dir: &Path) -> StageResult<(PathBuf, f64)> {
        assert!(source.is_file());
        self.transcodes.fetch_add(1, Ordering::SeqCst);
        let mxf = dir.join("out.mxf");
        std::fs::write(&mxf, b"mxf").unwrap();
        Ok((mxf, 12.0))
    }

    async fn verify(&self, _target: &TargetRow, mxf: &Path, _duration: f64) -> StageResult<bool> {
        self.dir_existed_at_verify.store(mxf.is_file(), Ordering::SeqCst);
        Ok(true)
    }
}

fn setup(fake: Fake) -> (Root, Repository, Arc<Fake>, Benchmark) {
    let root = Root::new();
    let paths = root.paths();
    paths.ensure_dirs().unwrap();
    let repo = Repository::new(paths.data.join("omni.db")).unwrap();
    let fake = Arc::new(fake);
    let bench = Benchmark::new(repo.clone(), paths, fake.clone(), "cli");
    (root, repo, fake, bench)
}

fn opts(mode: Mode, targets: Vec<i64>, parallel: usize) -> RunOptions {
    RunOptions { mode, targets, parallel, force: false }
}

fn quiet() -> OnTarget {
    Arc::new(|_, _, _, _| {})
}

fn count(root: &Root, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(root.paths().data.join("omni.db")).unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn running_job(repo: &Repository) -> i64 {
    let job = omni_core::models::NewJob::new("https://news.example/story/1", "1_TEST_JOB", "Test");
    repo.enqueue(&job, 24).unwrap();
    repo.lease_job("worker", 180).unwrap().expect("leased").id
}

#[tokio::test]
async fn a_full_run_leaves_the_queue_as_it_found_it() {
    let (root, repo, fake, bench) = setup(Fake::default());
    // One real job beforehand, so "no new row" cannot pass on an empty table.
    let job_id = running_job(&repo);
    assert_eq!(count(&root, "SELECT COUNT(*) FROM queue"), 1);
    let events_before = count(&root, "SELECT COUNT(*) FROM job_events");

    let o = RunOptions { force: true, ..opts(Mode::Full, vec![1, 2, 3], 2) };
    let run = bench.begin(&o).unwrap();
    let summary = bench.execute(run, &o, quiet()).await.unwrap();

    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 3, "the full run did download");
    assert_eq!(summary.delivery_ok, Some(3), "and went all the way through verify");
    assert_eq!(count(&root, "SELECT COUNT(*) FROM queue"), 1, "a benchmark created a queue row");
    assert_eq!(count(&root, "SELECT COUNT(*) FROM job_events"), events_before, "a benchmark wrote a job event");
    // The job that was running is still its worker's, untouched.
    assert_eq!(repo.get_job(job_id).unwrap().unwrap().status, omni_core::models::JobStatus::Running);
    // Nothing was delivered or archived.
    let paths = root.paths();
    assert!(std::fs::read_dir(&paths.archive).unwrap().next().is_none(), "the archive was written");
}

#[tokio::test]
async fn every_targets_directory_is_removed_after_it_finishes_even_when_the_download_failed() {
    let (root, _repo, fake, bench) = setup(Fake { no_download: vec![2], ..Default::default() });
    let o = opts(Mode::Full, vec![1, 2], 1);
    let run = bench.begin(&o).unwrap();
    let summary = bench.execute(run, &o, quiet()).await.unwrap();

    assert!(fake.dir_existed_at_verify.load(Ordering::SeqCst), "the stages did work inside the directory");
    let seen = fake.dirs_seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    for dir in &seen {
        assert!(dir.starts_with(root.paths().benchmark_temp()), "{dir:?} is outside temp/benchmark");
        assert!(!dir.exists(), "{dir:?} was left behind");
    }
    assert!(!root.paths().benchmark_temp().join(run.to_string()).exists(), "the run's own directory was left behind");
    assert_eq!(summary.failed, vec![2]);
}

#[tokio::test]
async fn extract_mode_downloads_nothing_and_makes_no_directory() {
    let (root, _repo, fake, bench) = setup(Fake::default());
    let o = opts(Mode::Extract, vec![1, 2, 3, 4], 2);
    let run = bench.begin(&o).unwrap();
    let summary = bench.execute(run, &o, quiet()).await.unwrap();
    assert_eq!(fake.extracted.load(Ordering::SeqCst), 4);
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 0);
    assert_eq!(summary.extract_ok, 4);
    assert_eq!(std::fs::read_dir(root.paths().benchmark_temp()).unwrap().count(), 0);
}

#[tokio::test]
async fn no_more_targets_run_at_once_than_asked_and_the_limit_is_reached() {
    for parallel in [1usize, 2, 4] {
        let (_root, _repo, fake, bench) = setup(Fake { extract_delay_ms: 40, ..Default::default() });
        let o = opts(Mode::Extract, (1..=12).collect(), parallel);
        let run = bench.begin(&o).unwrap();
        bench.execute(run, &o, quiet()).await.unwrap();
        assert_eq!(fake.peak.load(Ordering::SeqCst), parallel, "peak concurrency with --parallel {parallel}");
    }
}

#[tokio::test]
async fn more_than_four_at_once_is_refused() {
    let (_root, repo, _fake, bench) = setup(Fake::default());
    for parallel in [0usize, 5, 64] {
        assert!(matches!(bench.begin(&opts(Mode::Extract, vec![1], parallel)), Err(BeginError::Refused(_))), "{parallel}");
    }
    assert!(repo.benchmark_runs(10).unwrap().is_empty(), "a refused start leaves no run behind");
}

#[tokio::test]
async fn a_running_job_refuses_the_start_and_force_runs_anyway() {
    let (_root, repo, fake, bench) = setup(Fake::default());
    running_job(&repo);

    let refused = bench.begin(&opts(Mode::Extract, vec![1], 2));
    assert!(matches!(&refused, Err(BeginError::Refused(why)) if why.contains("1 job(s) are running")), "{refused:?}");
    assert!(repo.benchmark_runs(10).unwrap().is_empty());

    // The control: the same request with force runs.
    let o = RunOptions { force: true, ..opts(Mode::Extract, vec![1], 2) };
    let run = bench.begin(&o).expect("force must run while a job runs");
    bench.execute(run, &o, quiet()).await.unwrap();
    assert_eq!(fake.extracted.load(Ordering::SeqCst), 1);
    assert_eq!(repo.benchmark_run(run).unwrap().unwrap().state, "done");
}

#[tokio::test]
async fn one_run_at_a_time_and_a_run_left_by_a_dead_process_does_not_block_for_ever() {
    let (_root, repo, _fake, bench) = setup(Fake::default());
    let first = bench.begin(&opts(Mode::Extract, vec![1], 2)).unwrap();
    let again = bench.begin(&opts(Mode::Full, vec![1], 2));
    assert!(matches!(&again, Err(BeginError::Refused(why)) if why.contains(&first.to_string())), "{again:?}");

    // A `running` row from seven hours ago was left by a process that died.
    let (_root2, repo2, _fake2, bench2) = setup(Fake::default());
    let BenchStart::Started(old) = repo2
        .begin_benchmark_run(Mode::Extract, "{}", Utc::now() - ChronoDuration::hours(7), ChronoDuration::hours(6), "cli")
        .unwrap()
    else {
        panic!()
    };
    let fresh = bench2.begin(&opts(Mode::Extract, vec![1], 2)).expect("a stale run must not block");
    assert_ne!(old, fresh);
    assert_eq!(repo2.benchmark_run(old).unwrap().unwrap().state, "abandoned");
    drop(repo);
}

#[tokio::test]
async fn no_stream_address_reaches_the_database_or_the_report_files() {
    let (root, repo, _fake, bench) = setup(Fake { no_video: vec![3], ..Default::default() });
    let o = opts(Mode::Full, vec![1, 2, 3], 2);
    let run = bench.begin(&o).unwrap();
    bench.execute(run, &o, quiet()).await.unwrap();

    // The host is kept; the path, the token and the page address are not.
    let results = repo.benchmark_results(run).unwrap();
    assert!(results.iter().any(|r| r.stream_host.as_deref() == Some("cdn.example.net")));
    let dir = root.paths().benchmark_report_dir(run);
    let json = std::fs::read_to_string(dir.join("report.json")).expect("report.json is written");
    let md = std::fs::read_to_string(dir.join("report.md")).expect("report.md is written");
    let conn = rusqlite::Connection::open(root.paths().data.join("omni.db")).unwrap();
    let mut stored = String::new();
    for sql in [
        "SELECT params_json || ifnull(summary_json,'') FROM benchmark_runs",
        "SELECT ifnull(method,'') || ifnull(error_code,'') || ifnull(stream_host,'') FROM benchmark_results",
    ] {
        let mut stmt = conn.prepare(sql).unwrap();
        for s in stmt.query_map([], |r| r.get::<_, String>(0)).unwrap() {
            stored.push_str(&s.unwrap());
        }
    }
    for (what, text) in [("database", &stored), ("report.json", &json), ("report.md", &md)] {
        for needle in ["https://", "http://", "secret-path", "master.m3u8", "tok-8f31", "cookie-value-1", "news.example/story"] {
            assert!(!text.contains(needle), "{what} holds {needle}");
        }
    }
    assert!(md.contains("cdn.example.net") && md.contains("NO_STREAM_FOUND"));
}

#[tokio::test]
async fn the_report_judges_the_run_and_compares_it_with_the_last_run_of_its_mode() {
    let (_root, repo, _fake, bench) = setup(Fake { no_video: vec![5], ..Default::default() });
    let o = opts(Mode::Extract, (1..=20).collect(), 2);
    let first = bench.begin(&o).unwrap();
    let s1 = bench.execute(first, &o, quiet()).await.unwrap();
    // 19 of 20 is 95 %: on the threshold, so it passes.
    assert_eq!(s1.extract_pct, Some(95.0));
    assert_eq!(s1.checks[0].pass, Some(true));
    assert!(benchmark::build_report(&repo, first).unwrap().unwrap().deltas.is_none(), "nothing to compare with yet");

    // A full run in between is not what the next extract run is compared with.
    let full = opts(Mode::Full, vec![1, 2], 2);
    let mid = bench.begin(&full).unwrap();
    bench.execute(mid, &full, quiet()).await.unwrap();

    let second = bench.begin(&o).unwrap();
    bench.execute(second, &o, quiet()).await.unwrap();
    let report = benchmark::build_report(&repo, second).unwrap().unwrap();
    assert_eq!(report.deltas.as_ref().map(|d| d.previous_run_id), Some(first));
    assert_eq!(report.deltas.unwrap().extract_pct, Some(0.0));
}

#[tokio::test]
async fn the_hub_page_expected_to_fail_does_not_fail_the_run() {
    let (_root, _repo, _fake, bench) = setup(Fake { no_video: vec![61], ..Default::default() });
    let o = opts(Mode::Extract, vec![60, 61, 62], 2);
    let run = bench.begin(&o).unwrap();
    let s = bench.execute(run, &o, quiet()).await.unwrap();
    assert_eq!((s.targets_run, s.counted), (3, 2));
    assert_eq!(s.extract_pct, Some(100.0));
    assert!(s.failed.is_empty());
}

fn finish_job(repo: &Repository, job_id: i64) {
    repo.finish(job_id, "worker", omni_core::models::JobStatus::Completed, None, None, None).unwrap();
}

#[tokio::test]
async fn a_job_that_starts_mid_run_pauses_the_next_target_until_it_ends() {
    let (_root, repo, fake, bench) = setup(Fake::default());
    let bench = Arc::new(bench.with_poll(Duration::from_millis(20)));
    // After target 1 is downloaded, an on-air job starts.
    let job = Arc::new(std::sync::atomic::AtomicI64::new(0));
    {
        let (repo, job) = (repo.clone(), job.clone());
        *fake.after_download.lock().unwrap() = Some(Box::new(move |idx| {
            if idx == 1 {
                job.store(running_job(&repo), Ordering::SeqCst);
            }
        }));
    }
    let o = opts(Mode::Full, vec![1, 2], 1);
    let run = bench.begin(&o).unwrap();
    let task = {
        let (bench, o) = (bench.clone(), o.clone());
        tokio::spawn(async move { bench.execute(run, &o, quiet()).await })
    };
    // The run reads paused, and target 2 has not been downloaded while the job runs.
    let mut paused = false;
    for _ in 0..250 {
        paused = repo.benchmark_run(run).unwrap().unwrap().paused;
        if paused {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(paused, "the run never paused for the running job");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 1, "a target started while a job was running");
    assert_eq!(fake.transcodes.load(Ordering::SeqCst), 0, "an encode started while a job was running");
    assert_eq!(repo.benchmark_run(run).unwrap().unwrap().state, "running");

    finish_job(&repo, job.load(Ordering::SeqCst));
    let summary = task.await.unwrap().unwrap();
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 2, "the run resumed after the job ended");
    assert_eq!(summary.delivery_ok, Some(2));
    let row = repo.benchmark_run(run).unwrap().unwrap();
    assert_eq!((row.state.as_str(), row.paused), ("done", false));
}

#[tokio::test]
async fn force_does_not_pause_for_a_running_job() {
    let (_root, repo, fake, bench) = setup(Fake::default());
    let bench = bench.with_poll(Duration::from_millis(20));
    {
        let repo = repo.clone();
        *fake.after_download.lock().unwrap() = Some(Box::new(move |idx| {
            if idx == 1 {
                running_job(&repo);
            }
        }));
    }
    let o = RunOptions { force: true, ..opts(Mode::Full, vec![1, 2], 1) };
    let run = bench.begin(&o).unwrap();
    let done = tokio::time::timeout(Duration::from_secs(10), bench.execute(run, &o, quiet())).await;
    let summary = done.expect("a forced run must not wait for the job").unwrap();
    assert_eq!(summary.delivery_ok, Some(2));
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_source_over_ten_minutes_is_skipped_not_failed_and_not_transcoded() {
    // Production has no length cap: a long target must not drag the delivery rate down.
    let (_root, repo, fake, bench) = setup(Fake { long: vec![1], ..Default::default() });
    let o = opts(Mode::Full, vec![1, 2, 3], 1);
    let run = bench.begin(&o).unwrap();
    let s = bench.execute(run, &o, quiet()).await.unwrap();
    assert_eq!(fake.transcodes.load(Ordering::SeqCst), 2, "the long source was transcoded");
    assert!(s.failed.is_empty(), "a skipped target is not a failure: {:?}", s.failed);
    assert_eq!(s.skipped, vec![1]);
    assert_eq!(s.compliance_fails, Some(0), "and not a compliance failure");
    assert_eq!((s.delivery_ok, s.delivery_pct), (Some(2), Some(100.0)), "2 of 2, not 2 of 3");
    assert_eq!(s.checks[1].pass, Some(true));
    let report = benchmark::build_report(&repo, run).unwrap().unwrap();
    assert_eq!(report.targets[0].error_code.as_deref(), Some("SOURCE_TOO_LONG"));
    assert!(report.targets[0].skipped && !report.targets[1].skipped);
    assert_eq!((report.targets[0].download_ok, report.targets[0].verify_ok), (Some(true), None));
    assert!(benchmark::markdown(&report).contains("(skipped)"));
}

#[tokio::test]
async fn a_length_known_at_extraction_skips_the_download() {
    let (_root, _repo, fake, bench) = setup(Fake { known_secs: Some(3600.0), ..Default::default() });
    let o = opts(Mode::Full, vec![1], 1);
    let run = bench.begin(&o).unwrap();
    let s = bench.execute(run, &o, quiet()).await.unwrap();
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 0, "an hour-long video was downloaded");
    assert_eq!(fake.transcodes.load(Ordering::SeqCst), 0);
    assert_eq!(s.skipped, vec![1]);
    assert!(s.failed.is_empty());
    assert_eq!(s.extract_ok, 1, "it was found, which is what extraction is judged on");

    // The control: a known short length downloads as usual; exactly ten minutes too.
    for secs in [90.0, 600.0] {
        let (_root, _repo, fake, bench) = setup(Fake { known_secs: Some(secs), ..Default::default() });
        let run = bench.begin(&o).unwrap();
        let s = bench.execute(run, &o, quiet()).await.unwrap();
        assert_eq!(fake.downloaded.load(Ordering::SeqCst), 1, "{secs}");
        assert_eq!(s.delivery_ok, Some(1));
    }
}

#[tokio::test]
async fn a_run_that_errors_is_finished_as_failed() {
    let (root, repo, _fake, bench) = setup(Fake::default());
    let o = opts(Mode::Extract, vec![1], 1);
    let run = bench.begin(&o).unwrap();
    // The results table is gone: the run cannot be summarised.
    rusqlite::Connection::open(root.paths().data.join("omni.db"))
        .unwrap()
        .execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE benchmark_results;")
        .unwrap();
    assert!(bench.execute(run, &o, quiet()).await.is_err());
    assert_eq!(repo.benchmark_run(run).unwrap().unwrap().state, "failed", "the run was left running");
}

fn plant_cookie_file(paths: &AppPaths) -> PathBuf {
    let dir = paths.benchmark_target_temp(5, 7);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("cookies.txt");
    std::fs::write(&jar, b".x.com\tTRUE\t/\tTRUE\t0\tsid\tplanted-value\n").unwrap();
    std::fs::create_dir_all(paths.benchmark_temp().join("cookies")).unwrap();
    std::fs::write(paths.benchmark_temp().join("cookies").join("scratch.txt"), b"x").unwrap();
    jar
}

fn begin_as(repo: &Repository, owner: &str, minutes_ago: i64) -> i64 {
    let started = Utc::now() - ChronoDuration::minutes(minutes_ago);
    let BenchStart::Started(id) =
        repo.begin_benchmark_run(Mode::Full, "{}", started, ChronoDuration::hours(24), owner).unwrap()
    else {
        panic!()
    };
    id
}

#[tokio::test]
async fn start_up_abandons_a_daemon_run_at_once_and_removes_its_cookie_files() {
    let (root, repo, _fake, _bench) = setup(Fake::default());
    let paths = root.paths();
    let jar = plant_cookie_file(&paths);
    // Its heartbeat is fresh: the daemon that owned it is gone all the same.
    let dead = begin_as(&repo, "daemon", 0);
    startup_sweep(&repo, &paths);
    assert_eq!(repo.benchmark_run(dead).unwrap().unwrap().state, "abandoned");
    assert!(!jar.exists(), "a station cookie jar was left in plaintext");
    assert!(!paths.benchmark_temp().exists());
    // Start is not blocked.
    let next = repo.begin_benchmark_run(Mode::Extract, "{}", Utc::now(), ChronoDuration::minutes(5), "daemon").unwrap();
    assert!(matches!(next, BenchStart::Started(_)));
}

#[tokio::test]
async fn start_up_leaves_a_live_command_line_run_and_its_directory_alone() {
    let (root, repo, _fake, _bench) = setup(Fake::default());
    let paths = root.paths();
    let jar = plant_cookie_file(&paths);
    let live = begin_as(&repo, "cli", 1);
    startup_sweep(&repo, &paths);
    assert_eq!(repo.benchmark_run(live).unwrap().unwrap().state, "running");
    assert!(jar.exists(), "a live run's files were removed");
}

#[tokio::test]
async fn start_up_abandons_a_command_line_run_silent_for_six_minutes() {
    let (root, repo, _fake, _bench) = setup(Fake::default());
    let paths = root.paths();
    let jar = plant_cookie_file(&paths);
    let stale = begin_as(&repo, "cli", 6);
    startup_sweep(&repo, &paths);
    assert_eq!(repo.benchmark_run(stale).unwrap().unwrap().state, "abandoned");
    assert!(!jar.exists());
}

#[tokio::test]
async fn a_job_running_before_the_download_holds_the_download_back() {
    let (_root, repo, fake, bench) = setup(Fake::default());
    let bench = Arc::new(bench.with_poll(Duration::from_millis(20)));
    // The on-air job starts while target 1 is being extracted, before its download.
    let job = Arc::new(std::sync::atomic::AtomicI64::new(0));
    {
        let (repo, job) = (repo.clone(), job.clone());
        *fake.during_extract.lock().unwrap() = Some(Box::new(move |_| {
            if job.load(Ordering::SeqCst) == 0 {
                job.store(running_job(&repo), Ordering::SeqCst);
            }
        }));
    }
    let o = opts(Mode::Full, vec![1], 1);
    let run = bench.begin(&o).unwrap();
    let task = {
        let (bench, o) = (bench.clone(), o.clone());
        tokio::spawn(async move { bench.execute(run, &o, quiet()).await })
    };
    let mut paused = false;
    for _ in 0..250 {
        paused = repo.benchmark_run(run).unwrap().unwrap().paused;
        if paused {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(paused, "the run never paused for the running job");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 0, "a download started while a job was running");

    finish_job(&repo, job.load(Ordering::SeqCst));
    let summary = task.await.unwrap().unwrap();
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 1);
    assert_eq!(summary.delivery_ok, Some(1));
}

#[tokio::test]
async fn a_closed_update_gate_holds_back_the_probe_and_the_download_and_a_pass_is_held_during_them() {
    let (_root, _repo, fake, bench) = setup(Fake::default());
    let gate = UpdateGate::new();
    let bench = Arc::new(bench.with_poll(Duration::from_millis(20)).with_gate(gate.clone()));
    *fake.watch_gate.lock().unwrap() = Some(gate.clone());
    assert!(gate.pause_and_drain(Duration::from_secs(1)).await, "the gate closes");

    let o = opts(Mode::Full, vec![1], 1);
    let run = bench.begin(&o).unwrap();
    let task = {
        let (bench, o) = (bench.clone(), o.clone());
        tokio::spawn(async move { bench.execute(run, &o, quiet()).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.extracted.load(Ordering::SeqCst), 0, "yt-dlp was probed while the updater held the gate");
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 0, "a download started while the updater held the gate");

    gate.resume();
    let summary = task.await.unwrap().unwrap();
    assert_eq!(summary.delivery_ok, Some(1));
    assert_eq!(fake.gate_active_at_download.load(Ordering::SeqCst), 1, "no pass was held during the download");
    assert_eq!(gate.active(), 0, "a pass was left behind");
}

#[tokio::test]
async fn a_closed_gate_waits_between_the_probe_and_the_download_too() {
    // The updater closes the gate after extraction ends: the download waits for it.
    let (_root, _repo, fake, bench) = setup(Fake::default());
    let gate = UpdateGate::new();
    let bench = Arc::new(bench.with_poll(Duration::from_millis(20)).with_gate(gate.clone()));
    {
        let gate = gate.clone();
        *fake.during_extract.lock().unwrap() = Some(Box::new(move |_| {
            // Closing needs no await when nothing else is in flight but this probe's own pass:
            // raise the flag directly the way the updater does, without waiting for the drain.
            let g = gate.clone();
            tokio::spawn(async move {
                let _ = g.pause_and_drain(Duration::from_millis(1)).await;
            });
        }));
    }
    let o = opts(Mode::Full, vec![1], 1);
    let run = bench.begin(&o).unwrap();
    let task = {
        let (bench, o) = (bench.clone(), o.clone());
        tokio::spawn(async move { bench.execute(run, &o, quiet()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 0, "a download started while the gate was closed");
    gate.resume();
    task.await.unwrap().unwrap();
    assert_eq!(fake.downloaded.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_retention_sweep_removes_idle_scratch_and_spares_a_running_run() {
    let (root, repo, _fake, _bench) = setup(Fake::default());
    let paths = root.paths();
    let jar = plant_cookie_file(&paths);
    // A run is going: nothing is removed.
    let live = begin_as(&repo, "cli", 0);
    sweep_idle_scratch(&repo, &paths);
    assert!(jar.exists(), "a running run's files were removed");
    // The run ends (or was killed and abandoned): the next sweep clears the copies.
    repo.finish_benchmark_run(live, "done", None, Utc::now()).unwrap();
    sweep_idle_scratch(&repo, &paths);
    assert!(!jar.exists(), "cookie copies were left with no run going");
    assert!(!paths.benchmark_temp().exists());
    sweep_idle_scratch(&repo, &paths); // nothing there: no error
}
