use omni_core::config::AppConfig;
use omni_core::repository::Repository;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::ratelimit::LoginRateLimiter;

/// What «Run now» on the admin's Sites card got (P10.15a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunNow {
    /// The check runs in the background.
    Started,
    /// Another canary run, or the yt-dlp update, holds the lock.
    Busy,
    /// The site was started less than the cooldown ago; this many seconds remain.
    Cooldown(u64),
}

/// Starts one canary check of a site (and the self-heal when it is broken and
/// repair is on). The daemon implements it; the web crate cannot see the canary
/// code. Never waits for the check.
pub trait SiteRunner: Send + Sync {
    fn run_now(&self, domain: &str) -> RunNow;
}

/// Why a Test URL run did not happen (P10.15b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestUrlError {
    /// Another test is running; only one at a time in the process.
    Busy,
    /// The address may not be tested; the reason is short and names no path.
    Refused(String),
}

pub type TestUrlFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, TestUrlError>> + Send + 'a>>;

/// The admin's «Test URL»: a dry run of the normal path and the repair ladder
/// on one address. The daemon implements it; the web crate cannot see the repair
/// code. Nothing is downloaded, queued or delivered.
pub trait UrlTester: Send + Sync {
    fn test_url<'a>(&'a self, url: &'a str, use_llm: bool) -> TestUrlFuture<'a>;
}

pub type CookieTestFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(bool, String), String>> + Send + 'a>>;

/// The admin's cookie-jar «Test» (P3.5): runs yt-dlp with the site's jar on a
/// sample link and stores the outcome. The daemon implements it (it knows where
/// yt-dlp is). `Ok((ok, detail))`; `Err` is a reason the test could not start.
pub trait CookieTester: Send + Sync {
    fn test<'a>(&'a self, domain: &'a str, url: &'a str) -> CookieTestFuture<'a>;
}

/// What the Benchmarks card's «Start» asks for (P6.4).
pub struct BenchRequest {
    pub mode: omni_core::benchmark::Mode,
    /// Target numbers, already checked against the corpus.
    pub targets: Vec<i64>,
    pub parallel: u32,
    /// Run although a job is running.
    pub force: bool,
    /// Called when a target is done and when the run ends; the card refetches.
    pub notify: Arc<dyn Fn() + Send + Sync>,
}

/// Why a benchmark run did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchStartError {
    /// A job is running, or another run is; the reason is short and names no address.
    Refused(String),
    /// Anything else (the database); the reason is logged, not shown.
    Failed,
}

/// Starts a benchmark run in the background and returns its id. The daemon
/// implements it (it knows where the tools are); the web crate cannot see the
/// runner. Never waits for the run.
pub trait BenchmarkStarter: Send + Sync {
    fn start(&self, request: BenchRequest) -> Result<i64, BenchStartError>;
}

#[derive(Clone)]
pub struct AppState {
    pub repo: Repository,
    pub config: Arc<RwLock<AppConfig>>,
    pub config_path: PathBuf,
    /// Live-update bus shared with the worker side (P1.10); `/api/events` streams it.
    pub events: omni_core::events::EventBus,
    /// Shared across every request, so the counters are global to the process
    /// rather than per connection.
    pub login_limiter: Arc<LoginRateLimiter>,
    /// The encrypted secret store (plan P2.6).
    ///
    /// The panel can say whether a secret is *set* and can replace it; there
    /// is deliberately no route that reads one back out, so a compromised
    /// admin session cannot exfiltrate the mailbox password — only overwrite
    /// it, which is loud.
    pub secrets: Arc<omni_core::secrets::SecretStore>,
    /// Live subsystem health (plan P6.2).
    ///
    /// Written by the subsystems as they run and read by the status endpoints,
    /// so the panel reflects what actually happened rather than what a probe
    /// fired at page-load time would say.
    pub health: omni_core::health::HealthState,
    /// Whether the listener actually serves HTTPS.
    ///
    /// Drives the `Secure` cookie attribute. It is a property of the running
    /// listener, not of config alone: marking a cookie `Secure` on a
    /// plain-HTTP deployment means the browser never sends it back, which
    /// presents to operators as "login does nothing".
    pub tls_enabled: bool,
    /// The LLM assist in use, shared with the mail watcher: saving LLM
    /// settings in the admin panel swaps it without a restart (plan P4.22).
    pub llm: omni_email::assist::LiveAssist,
    /// Jobs a worker is still busy with; a retry waits until it has stopped
    /// (plan P7.20). Shared with the worker pool by the daemon.
    pub busy_jobs: omni_core::busy::BusyJobs,
    /// The daemon's temp directory, where `held/{id}` keeps a video waiting
    /// for approval (P10.5). `None` in tests that have none: a rejection then
    /// leaves the directory to the sweep.
    pub temp_dir: Option<PathBuf>,
    /// The canary runner behind «Run now» (P10.15a); `None` in tests that have none.
    pub site_runner: Option<Arc<dyn SiteRunner>>,
    /// The dry-run tester behind the admin's «Test URL» (P10.15b); `None` in tests that have none.
    pub url_tester: Option<Arc<dyn UrlTester>>,
    /// The cookie-jar tester behind the Cookies card (P3.5); `None` in tests that have none.
    pub cookie_tester: Option<Arc<dyn CookieTester>>,
    /// The benchmark runner behind the Benchmarks card (P6.4); `None` in tests that have none.
    pub benchmark_starter: Option<Arc<dyn BenchmarkStarter>>,
    /// The log redactor: an uploaded jar's values join it at once (P3.5).
    pub redactions: Option<omni_core::logging::Redactions>,
    /// The update gate the worker pool holds a pass on; a tool swap waits for it to drain (P9.3).
    pub update_gate: omni_core::update_gate::UpdateGate,
    /// The one running tool install, how each ended, and the hash cache (P9.3).
    pub tool_installer: omni_core::dependencies::ToolInstaller,
    /// Where pinned archives come from; HTTP in the daemon, bytes in tests.
    pub tool_fetcher: Arc<dyn omni_core::dependencies::Fetcher>,
    /// The daemon's `bin/`, anchored to the install dir; `None` in tests, where the config dir is used.
    pub bin_dir: Option<PathBuf>,
    /// Tools that were not in `bin/` when the daemon started: it cached a fallback path for them,
    /// so a new install is only used after a restart.
    pub tools_missing_at_start: Arc<Vec<String>>,
    /// Tool paths `config.json` set, as the daemon resolved them at start-up (name -> path).
    pub tool_overrides: Arc<std::collections::HashMap<String, PathBuf>>,
    /// The canary run lock: a canary's yt-dlp starts without a gate pass, so a yt-dlp or
    /// Deno swap holds it too, as the nightly task does. `None` in tests that have none.
    pub canary_lock: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl AppState {
    pub fn new(repo: Repository, config: AppConfig, config_path: PathBuf) -> Self {
        let limiter = LoginRateLimiter::new(
            config.security.login_rate_limit_per_5min,
            config.security.login_rate_limit_per_account_hour,
        );
        let tls_enabled = config.tls.is_enabled();
        let llm = omni_email::assist::LiveAssist::new(omni_email::assist::Assist::from_config(&config));
        // Tests and callers that do not configure secrets get a store rooted
        // beside the config; the daemon replaces it with the real one.
        let default_store = omni_core::secrets::SecretStore::new(
            config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("data")
                .join("secrets.bin"),
        );
        Self {
            repo,
            config: Arc::new(RwLock::new(config)),
            config_path,
            events: omni_core::events::EventBus::new(),
            login_limiter: Arc::new(limiter),
            secrets: Arc::new(default_store),
            health: omni_core::health::HealthState::new(),
            tls_enabled,
            llm,
            busy_jobs: omni_core::busy::BusyJobs::new(),
            temp_dir: None,
            site_runner: None,
            url_tester: None,
            cookie_tester: None,
            benchmark_starter: None,
            redactions: None,
            update_gate: omni_core::update_gate::UpdateGate::new(),
            tool_installer: omni_core::dependencies::ToolInstaller::new(),
            tool_fetcher: Arc::new(omni_core::dependencies::HttpFetcher),
            bin_dir: None,
            tools_missing_at_start: Arc::new(Vec::new()),
            tool_overrides: Arc::new(std::collections::HashMap::new()),
            canary_lock: None,
        }
    }

    /// Where held videos live, so a rejection can remove its file now.
    pub fn with_temp_dir(mut self, temp_dir: PathBuf) -> Self {
        self.temp_dir = Some(temp_dir);
        self
    }

    /// The canary runner for the Sites card's «Run now».
    pub fn with_site_runner(mut self, runner: Arc<dyn SiteRunner>) -> Self {
        self.site_runner = Some(runner);
        self
    }

    /// The dry-run tester for the admin's «Test URL».
    pub fn with_url_tester(mut self, tester: Arc<dyn UrlTester>) -> Self {
        self.url_tester = Some(tester);
        self
    }

    /// The cookie-jar tester for the Cookies card's «Test».
    pub fn with_cookie_tester(mut self, tester: Arc<dyn CookieTester>) -> Self {
        self.cookie_tester = Some(tester);
        self
    }

    /// The benchmark runner for the Benchmarks card.
    pub fn with_benchmark_starter(mut self, starter: Arc<dyn BenchmarkStarter>) -> Self {
        self.benchmark_starter = Some(starter);
        self
    }

    /// Share the worker pool's update gate, so a tool swap waits for running jobs.
    pub fn with_update_gate(mut self, gate: omni_core::update_gate::UpdateGate) -> Self {
        self.update_gate = gate;
        self
    }

    /// The daemon's `bin/` directory and the tools it found missing at start-up.
    pub fn with_tool_dirs(mut self, bin_dir: PathBuf, missing_at_start: Vec<String>) -> Self {
        self.bin_dir = Some(bin_dir);
        self.tools_missing_at_start = Arc::new(missing_at_start);
        self
    }

    /// The tool paths `config.json` sets, which the daemon runs instead of `bin/`.
    pub fn with_tool_overrides(mut self, overrides: Vec<(String, PathBuf)>) -> Self {
        self.tool_overrides = Arc::new(overrides.into_iter().collect());
        self
    }

    /// Share the daemon's install state, so its hash cache and the health line serve both.
    pub fn with_tool_installer(mut self, installer: omni_core::dependencies::ToolInstaller) -> Self {
        self.tool_installer = installer;
        self
    }

    /// The canary run lock a yt-dlp or Deno swap must hold.
    pub fn with_canary_lock(mut self, lock: Arc<tokio::sync::Mutex<()>>) -> Self {
        self.canary_lock = Some(lock);
        self
    }

    /// Replace where pinned archives are fetched from (tests).
    pub fn with_tool_fetcher(mut self, fetcher: Arc<dyn omni_core::dependencies::Fetcher>) -> Self {
        self.tool_fetcher = fetcher;
        self
    }

    /// The daemon's log redactor, so a jar uploaded while it runs is redacted at once.
    pub fn with_redactions(mut self, redactions: omni_core::logging::Redactions) -> Self {
        self.redactions = Some(redactions);
        self
    }

    /// Share the daemon's health state, so the endpoints report what the
    /// subsystems observed rather than a second, private copy.
    pub fn with_health(mut self, health: omni_core::health::HealthState) -> Self {
        self.health = health;
        self
    }

    /// Share the worker pool's busy-job set.
    pub fn with_busy_jobs(mut self, busy: omni_core::busy::BusyJobs) -> Self {
        self.busy_jobs = busy;
        self
    }

    /// Share the daemon's live LLM assist with the admin panel.
    pub fn with_llm(mut self, llm: omni_email::assist::LiveAssist) -> Self {
        self.llm = llm;
        self
    }

    /// Point the state at the daemon's real secret store.
    pub fn with_secret_store(mut self, store: omni_core::secrets::SecretStore) -> Self {
        self.secrets = Arc::new(store);
        self
    }

    /// Mark the listener as serving HTTPS, which turns on the `Secure` cookie
    /// attribute (plan P2.7).
    pub fn with_tls(mut self, enabled: bool) -> Self {
        self.tls_enabled = enabled;
        self
    }

    /// Share the daemon's event bus, the one the repository announces on.
    pub fn with_events(mut self, bus: omni_core::events::EventBus) -> Self {
        self.events = bus;
        self
    }

    /// Announce `{"type":"<event>"}` to the desks.
    pub fn broadcast_event(&self, event: &str) {
        self.events.event(event, None);
    }

    /// Same, naming the job so the desk can update just that card.
    pub fn broadcast_job_event(&self, event: &str, job_id: i64) {
        self.events.event(event, Some(job_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_event_sends_typed_json_and_job_events_carry_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let state = AppState::new(repo, AppConfig::default(), dir.path().join("config.json"));
        let mut rx = state.events.subscribe();
        state.broadcast_event("x");
        state.broadcast_job_event("job_updated", 12);
        let a: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        let b: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(a, serde_json::json!({"type": "x"}));
        assert_eq!(b, serde_json::json!({"type": "job_updated", "id": 12}));
    }
}
