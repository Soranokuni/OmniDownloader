use omni_core::config::AppConfig;
use omni_core::repository::Repository;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

#[derive(Clone)]
pub struct AppState {
    pub repo: Repository,
    pub config: Arc<RwLock<AppConfig>>,
    pub config_path: PathBuf,
    pub event_tx: broadcast::Sender<String>,
}

impl AppState {
    pub fn new(repo: Repository, config: AppConfig, config_path: PathBuf) -> Self {
        let (tx, _) = broadcast::channel(100);
        Self {
            repo,
            config: Arc::new(RwLock::new(config)),
            config_path,
            event_tx: tx,
        }
    }

    pub fn broadcast_event(&self, event: &str) {
        let _ = self.event_tx.send(event.to_string());
    }
}
