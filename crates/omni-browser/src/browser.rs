use anyhow::{Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use tracing::info;

pub struct HeadlessBrowserManager;

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

    pub async fn launch() -> Result<(Browser, tokio::task::JoinHandle<()>)> {
        let mut builder = BrowserConfig::builder();

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

        let (browser, mut handler) = Browser::launch(config)
            .await
            .context("Failed launching Chromium via CDP")?;

        let handle = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                let _ = event;
            }
        });

        Ok((browser, handle))
    }
}
