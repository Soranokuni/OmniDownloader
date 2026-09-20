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
    /// Whether the listener actually serves HTTPS.
    ///
    /// Drives the `Secure` cookie attribute. It is a property of the running
    /// listener, not of config alone: marking a cookie `Secure` on a
    /// plain-HTTP deployment means the browser never sends it back, which
    /// presents to operators as "login does nothing".
    pub tls_enabled: bool,
}

impl AppState {
    pub fn new(repo: Repository, config: AppConfig, config_path: PathBuf) -> Self {
        let (tx, _) = broadcast::channel(100);
        let limiter = LoginRateLimiter::new(
            config.security.login_rate_limit_per_5min,
            config.security.login_rate_limit_per_account_hour,
        );
        let tls_enabled = config.tls.is_enabled();
        Self {
            repo,
            config: Arc::new(RwLock::new(config)),
            config_path,
            event_tx: tx,
            login_limiter: Arc::new(limiter),
            tls_enabled,
        }
    }

    pub fn broadcast_event(&self, event: &str) {
        let _ = self.event_tx.send(event.to_string());
    }
}
