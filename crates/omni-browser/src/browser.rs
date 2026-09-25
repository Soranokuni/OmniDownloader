use anyhow::{Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{info, warn};

pub struct HeadlessBrowserManager;

/// A running browser with a profile directory of its own.
///
/// chromiumoxide's default profile is one fixed `%TEMP%\chromiumoxide-runner`
/// for every launch. Chrome allows one process per profile, so the second of
/// two concurrent sniffs handed its window to the first and exited, and the
/// job failed "Failed launching Chromium via CDP" (trial run 2026-09-25, two
/// workers sniffing X posts at once).
///
/// Call [`BrowserSession::shutdown`] when done; it closes Chrome and removes
/// the profile. `Drop` is the fallback for an early return: Chrome is killed
/// by chromiumoxide, and a profile it could not remove yet is swept by the
/// next launch.
pub struct BrowserSession {
    pub browser: Browser,
    handler: tokio::task::JoinHandle<()>,
    profile: PathBuf,
}

impl BrowserSession {
    pub fn profile_dir(&self) -> &Path {
        &self.profile
    }

    pub async fn shutdown(&mut self) {
        let closed = tokio::time::timeout(Duration::from_secs(5), self.browser.close()).await;
        if !matches!(closed, Ok(Ok(_))) {
            let _ = self.browser.kill().await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.browser.wait()).await;
        self.handler.abort();
        // Chrome's helper processes let go of the profile a moment after the
        // main process exits.
        for _ in 0..10 {
            if !self.profile.exists() || std::fs::remove_dir_all(&self.profile).is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        warn!("Browser profile {:?} still in use; the next launch removes it", self.profile);
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        self.handler.abort();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

/// Where this process's browser profiles live.
fn profiles_root() -> PathBuf {
    std::env::temp_dir().join("omni-sniffer")
}

/// A profile directory no other launch uses, in this process or another.
fn unique_profile_dir(root: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    root.join(format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// Remove profiles a crash or a still-running Chrome left behind. Only old
/// ones: a profile younger than a sniff could still be in use.
fn sweep_stale_profiles(root: &Path, older_than: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > older_than);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

impl HeadlessBrowserManager {
    pub fn find_system_browser() -> Option<PathBuf> {
        let candidates = [
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ];

        for c in candidates {
            let p = Path::new(c);
            if p.exists() {
                return Some(p.to_path_buf());
            }
        }
        None
    }

    pub async fn launch() -> Result<BrowserSession> {
        let root = profiles_root();
        sweep_stale_profiles(&root, Duration::from_secs(3600));
        let profile = unique_profile_dir(&root);
        std::fs::create_dir_all(&profile)
            .with_context(|| format!("Failed creating browser profile {profile:?}"))?;

        let mut builder = BrowserConfig::builder().user_data_dir(&profile);

        if let Some(edge_or_chrome) = Self::find_system_browser() {
            info!("Found host system browser for CDP automation: {:?}", edge_or_chrome);
            builder = builder.chrome_executable(edge_or_chrome);
        } else {
            info!("System browser not in default path. Letting chromiumoxide auto-detect browser.");
        }

        let config = builder
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--mute-audio")
            .arg("--no-sandbox")
            .arg("--disable-dev-shm-usage")
            .arg("--user-agent=Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")
            .build()
            .map_err(|e| anyhow::anyhow!("Failed building BrowserConfig: {}", e))?;

        let (browser, mut handler) = match Browser::launch(config).await {
            Ok(launched) => launched,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&profile);
                return Err(e).context("Failed launching Chromium via CDP");
            }
        };

        let handler = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                let _ = event;
            }
        });

        Ok(BrowserSession { browser, handler, profile })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_launch_gets_its_own_profile() {
        let root = Path::new(r"C:\Temp\omni-sniffer");
        let a = unique_profile_dir(root);
        let b = unique_profile_dir(root);
        assert_ne!(a, b);
        assert!(a.starts_with(root) && b.starts_with(root));
    }

    #[test]
    fn the_sweep_leaves_a_profile_in_use_alone() {
        let root = tempfile::tempdir().unwrap();
        let fresh = root.path().join("1-0");
        std::fs::create_dir_all(fresh.join("Default")).unwrap();
        sweep_stale_profiles(root.path(), Duration::from_secs(3600));
        assert!(fresh.exists());
        sweep_stale_profiles(root.path(), Duration::ZERO);
        assert!(!fresh.exists());
    }
}
