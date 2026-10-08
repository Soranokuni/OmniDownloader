use omni_core::config::AppConfig;
use omni_core::repository::Repository;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::ratelimit::LoginRateLimiter;

#[derive(Clone)]
pub struct AppState {
    pub repo: Repository,
    pub config: Arc<RwLock<AppConfig>>,
    pub config_path: PathBuf,
    pub event_tx: broadcast::Sender<String>,
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
}

impl AppState {
    pub fn new(repo: Repository, config: AppConfig, config_path: PathBuf) -> Self {
        let (tx, _) = broadcast::channel(100);
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
            event_tx: tx,
            login_limiter: Arc::new(limiter),
            secrets: Arc::new(default_store),
            health: omni_core::health::HealthState::new(),
            tls_enabled,
            llm,
            busy_jobs: omni_core::busy::BusyJobs::new(),
        }
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

    pub fn broadcast_event(&self, event: &str) {
        let _ = self.event_tx.send(event.to_string());
    }
}
