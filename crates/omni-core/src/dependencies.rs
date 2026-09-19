use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyStatus {
    pub name: String,
    pub installed: bool,
    pub path: String,
    pub version: String,
}

pub struct DependencyManager {
    bin_dir: PathBuf,
}

impl DependencyManager {
    pub fn new<P: AsRef<Path>>(bin_dir: P) -> Self {
        let p = bin_dir.as_ref().to_path_buf();
        let _ = std::fs::create_dir_all(&p);
        Self { bin_dir: p }
    }

    pub fn get_bin_dir(&self) -> &Path {
        &self.bin_dir
    }

    pub fn scan(&self) -> Vec<DependencyStatus> {
        let tools = [
            ("yt-dlp", "yt-dlp.exe", "--version"),
            ("ffmpeg", "ffmpeg.exe", "-version"),
            ("ffprobe", "ffprobe.exe", "-version"),
            ("bmxtranswrap", "bmxtranswrap.exe", ""),
        ];

        let mut results = Vec::new();
        for (name, filename, ver_flag) in tools {
            let path = self.bin_dir.join(filename);
            if path.exists() {
                let ver = self.get_tool_version(&path, name, ver_flag);
                results.push(DependencyStatus {
                    name: name.to_string(),
                    installed: true,
                    path: path.to_string_lossy().to_string(),
                    version: ver,
                });
            } else {
                results.push(DependencyStatus {
                    name: name.to_string(),
                    installed: false,
                    path: "Missing".to_string(),
                    version: "Missing".to_string(),
                });
            }
        }
        results
    }

    pub fn find_binary(&self, name: &str) -> Option<PathBuf> {
        let exe_name = if name.ends_with(".exe") {
            name.to_string()
        } else {
            format!("{}.exe", name)
        };
        let candidate = self.bin_dir.join(&exe_name);
        if candidate.exists() {
            Some(candidate)
        } else {
            // Check system PATH
            which(&exe_name)
        }
    }

    fn get_tool_version(&self, path: &Path, name: &str, flag: &str) -> String {
        let mut cmd = Command::new(path);
        if !flag.is_empty() {
            cmd.arg(flag);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        match cmd.output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = format!("{}\n{}", stdout, stderr);

                if name == "yt-dlp" {
                    stdout.lines().next().unwrap_or("Installed").trim().to_string()
                } else if name == "ffmpeg" || name == "ffprobe" {
                    let re = Regex::new(r"version\s+([^\s]+)").unwrap();
                    if let Some(caps) = re.captures(&combined) {
                        caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_else(|| "Installed".into())
                    } else {
                        "Installed".into()
                    }
                } else if name == "bmxtranswrap" {
                    let re = Regex::new(r"bmx\s+v([^\s,]+)").unwrap();
                    if let Some(caps) = re.captures(&combined) {
                        caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_else(|| "Installed".into())
                    } else {
                        "Installed".into()
                    }
                } else {
                    "Installed".into()
                }
            }
            Err(_) => "Error running binary".into(),
        }
    }

    pub async fn check_ytdl_update(&self, channel: &str) -> Result<(bool, String, String)> {
        let local_path = self.bin_dir.join("yt-dlp.exe");
        let local_ver = if local_path.exists() {
            self.get_tool_version(&local_path, "yt-dlp", "--version")
        } else {
            "Missing".into()
        };

        let repo = if channel == "nightly" {
            "yt-dlp/yt-dlp-nightly-builds"
        } else {
            "yt-dlp/yt-dlp"
        };
        let api_url = format!("https://api.github.com/repos/{}/releases/latest", repo);

        let client = reqwest::Client::builder()
            .user_agent("OmniDownloader-Rust/1.0")
            .build()?;

        let resp = client.get(&api_url).send().await?.json::<serde_json::Value>().await?;
        let remote_ver = resp.get("tag_name").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let clean_remote = remote_ver.trim_start_matches('v').trim();
        let clean_local = local_ver.trim_start_matches('v').trim();

        let update_available = !clean_remote.is_empty() && clean_remote != clean_local && clean_local != "Missing";
        Ok((update_available, remote_ver, local_ver))
    }

    pub async fn download_ytdl(&self, channel: &str) -> Result<PathBuf> {
        let url = if channel == "nightly" {
            "https://github.com/yt-dlp/yt-dlp-nightly-builds/releases/latest/download/yt-dlp.exe"
        } else {
            "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"
        };

        let dest = self.bin_dir.join("yt-dlp.exe");
        let temp_dest = self.bin_dir.join("yt-dlp.exe.tmp");

        info!("Downloading yt-dlp from {}", url);
        let client = reqwest::Client::builder().user_agent("OmniDownloader-Rust/1.0").build()?;
        let bytes = client.get(url).send().await?.bytes().await?;

        tokio::fs::write(&temp_dest, bytes).await?;
        if dest.exists() {
            let _ = tokio::fs::remove_file(&dest).await;
        }
        tokio::fs::rename(&temp_dest, &dest).await?;
        info!("yt-dlp updated successfully at {:?}", dest);
        Ok(dest)
    }
}

fn which(exe_name: &str) -> Option<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
