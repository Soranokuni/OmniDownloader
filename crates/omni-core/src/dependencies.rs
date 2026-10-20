use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::info;

use crate::toolpins::ToolPin;
use crate::update_gate::UpdateGate;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyStatus {
    pub name: String,
    pub installed: bool,
    pub path: String,
    pub version: String,
}

pub struct DependencyManager {
    bin_dir: PathBuf,
    /// Where Windows keeps the VC++ runtime (`%SystemRoot%\System32`).
    system_dir: PathBuf,
    /// Replaces the version probe of a pinned install (tests: a fake exe cannot run).
    probe_hook: Option<ProbeHook>,
}

/// `true` when the exe at the path passes the probe.
pub type ProbeHook = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

impl DependencyManager {
    pub fn new<P: AsRef<Path>>(bin_dir: P) -> Self {
        let p = bin_dir.as_ref().to_path_buf();
        let _ = std::fs::create_dir_all(&p);
        let system_dir = std::env::var_os("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32"))
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"));
        Self { bin_dir: p, system_dir, probe_hook: None }
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
            // yt-dlp's JavaScript runtime for YouTube (plan P6.8).
            ("deno", "deno.exe", "--version"),
        ];

        let mut results = Vec::new();
        for (name, filename, ver_flag) in tools {
            let path = if name == "yt-dlp" { self.ytdl_live() } else { self.bin_dir.join(filename) };
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
        if name.trim_end_matches(".exe") == "yt-dlp" && self.ytdl_is_folder_build() {
            return Some(self.ytdl_live());
        }
        let exe_name = if name.ends_with(".exe") {
            name.to_string()
        } else {
            format!("{}.exe", name)
        };
        let candidate = self.bin_dir.join(&exe_name);
        if candidate.exists() {
            Some(candidate)
        } else if crate::toolpins::pin_for(&exe_name).is_some() && self.bin_dir.join(format!("{exe_name}.prev")).is_file() {
            // A pinned tool that was removed after a swap: missing, not the previous build and
            // not whatever PATH has. The pinned hash is the only authority (P9.3).
            None
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

                if name == "yt-dlp" || name == "deno" {
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
        let local_path = self.ytdl_live();
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

    // ------------------------------------------------------------------
    // yt-dlp, as the one-folder build (plan P2.10)
    // ------------------------------------------------------------------
    //
    // The one-file `yt-dlp.exe` is a PyInstaller archive that unpacks
    // itself into %TEMP% on *every* start: 4 s on the MCR machine, 18 s when
    // Defender scans the unpacked files, and it sometimes leaves the
    // directory behind ("Failed to remove temporary directory"). Every job
    // starts yt-dlp two or three times. The release's `yt-dlp_win.zip` is
    // the same program already unpacked: it starts in about 1 s.
    //
    // Live is the folder `bin/yt-dlp/`, staged `bin/yt-dlp.staged/`,
    // previous `bin/yt-dlp.prev/`; each carries the verified zip's hash in
    // `.sha256`, which is how a build is recognised. A legacy
    // `bin/yt-dlp.exe` is used while no folder build is installed.

    pub fn ytdl_dir(&self) -> PathBuf {
        self.bin_dir.join("yt-dlp")
    }
    fn ytdl_dir_staged(&self) -> PathBuf {
        self.bin_dir.join("yt-dlp.staged")
    }
    fn ytdl_dir_previous(&self) -> PathBuf {
        self.bin_dir.join("yt-dlp.prev")
    }
    /// The one-file build, used until a folder build is installed.
    pub fn ytdl_legacy(&self) -> PathBuf {
        self.bin_dir.join("yt-dlp.exe")
    }

    /// The yt-dlp every caller runs: the folder build when installed, else
    /// the legacy one-file exe. The same path before and after an update or
    /// a rollback, which swap whole folders under it.
    pub fn ytdl_live(&self) -> PathBuf {
        ytdl_exe_in(&self.bin_dir)
    }

    /// The executable inside a staged update.
    pub fn ytdl_staged(&self) -> PathBuf {
        self.ytdl_dir_staged().join("yt-dlp.exe")
    }

    /// Whether the live yt-dlp is the folder build.
    pub fn ytdl_is_folder_build(&self) -> bool {
        self.ytdl_dir().join("yt-dlp.exe").is_file()
    }

    /// The hash that identifies the live build: the verified zip's for a
    /// folder build, the exe's own for the legacy one-file build.
    pub fn ytdl_live_sha256(&self) -> Option<String> {
        if self.ytdl_is_folder_build() {
            std::fs::read_to_string(self.ytdl_dir().join(SHA_MARKER)).ok().map(|s| s.trim().to_string())
        } else {
            std::fs::read(self.ytdl_legacy()).ok().map(|b| sha256_hex(&b))
        }
    }

    /// Throw away a staged build that will not be applied.
    pub fn discard_staged_ytdl(&self) {
        let _ = std::fs::remove_dir_all(self.ytdl_dir_staged());
    }

    /// Download and verify a new yt-dlp, leaving it *staged* (plan P2.9, D-16).
    ///
    /// Three things the old `download_ytdl` did not do:
    ///
    /// 1. **Verify.** It wrote whatever came back from the network straight
    ///    over the live binary. `SHA2-256SUMS` comes from the same release, and
    ///    a mismatch aborts before anything is swapped. This is not protection
    ///    against a compromised GitHub — the checksum lives there too — but it
    ///    does catch the realistic failures: a truncated download, a captive
    ///    portal or proxy error page saved as an `.exe`, a corrupted transfer.
    /// 2. **Stage.** Overwriting the live binary while a job was downloading
    ///    could pull the executable out from under a running process.
    /// 3. **Keep the old one.** With no copy of the previous build, a yt-dlp
    ///    release that breaks an extractor left the newsroom with no way back
    ///    except finding the old exe by hand.
    ///
    /// Returns the staged executable and the verified hash.
    pub async fn stage_ytdl(&self, channel: &str) -> Result<StagedTool> {
        self.stage_ytdl_release(channel, None).await
    }

    /// [`Self::stage_ytdl`] for release `tag` (`None`: the latest).
    pub async fn stage_ytdl_release(&self, channel: &str, tag: Option<&str>) -> Result<StagedTool> {
        let repo = if channel == "nightly" { "yt-dlp/yt-dlp-nightly-builds" } else { "yt-dlp/yt-dlp" };
        let base = match tag {
            Some(t) => format!("https://github.com/{repo}/releases/download/{t}"),
            None => format!("https://github.com/{repo}/releases/latest/download"),
        };
        let zip_url = format!("{base}/{YTDL_ZIP}");
        let sums_url = format!("{base}/SHA2-256SUMS");

        let client = reqwest::Client::builder()
            .user_agent("OmniDownloader-Rust/1.0")
            .timeout(std::time::Duration::from_secs(300))
            .build()?;

        info!("Fetching yt-dlp checksums from {}", sums_url);
        let sums = client
            .get(&sums_url)
            .send()
            .await
            .context("Failed fetching SHA2-256SUMS")?
            .error_for_status()
            .context("SHA2-256SUMS request failed")?
            .text()
            .await?;

        let expected = expected_sha256(&sums, YTDL_ZIP).ok_or_else(|| {
            anyhow::anyhow!(
                "SHA2-256SUMS from the yt-dlp release has no entry for {YTDL_ZIP}; refusing to \
                 install an unverified build"
            )
        })?;

        info!("Downloading yt-dlp from {}", zip_url);
        let bytes = client
            .get(&zip_url)
            .send()
            .await
            .with_context(|| format!("Failed downloading {YTDL_ZIP}"))?
            .error_for_status()
            .with_context(|| format!("{YTDL_ZIP} request failed"))?
            .bytes()
            .await?;

        let actual = sha256_hex(&bytes);
        if actual != expected {
            anyhow::bail!(
                "{YTDL_ZIP} failed checksum verification (expected {expected}, got {actual}, \
                 {} bytes). Nothing was installed.",
                bytes.len()
            );
        }

        let staged_dir = self.ytdl_dir_staged();
        let sha = actual.clone();
        let into = staged_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _ = std::fs::remove_dir_all(&into);
            extract_zip_into(&bytes, &into)?;
            if !into.join("yt-dlp.exe").is_file() {
                let _ = std::fs::remove_dir_all(&into);
                anyhow::bail!("{YTDL_ZIP} has no yt-dlp.exe at its top level; nothing was installed");
            }
            std::fs::write(into.join(SHA_MARKER), &sha).with_context(|| format!("Failed writing into {into:?}"))?;
            Ok(())
        })
        .await
        .context("yt-dlp unpack task")??;

        let staged = self.ytdl_staged();
        info!("Staged a verified yt-dlp (sha256 {}) at {:?}", &actual[..16], staged_dir);
        Ok(StagedTool {
            path: staged,
            sha256: actual,
            bytes: dir_size(&staged_dir),
        })
    }

    /// Swap a staged yt-dlp into place, keeping the old one as `.prev`.
    ///
    /// The caller is responsible for there being no download in flight; see
    /// the update gate in `main.rs`: a folder whose exe or DLLs are in use
    /// cannot be renamed. Rename-based, so the window in which no build is
    /// at the live path is as short as the filesystem allows. Returns the
    /// live executable.
    pub fn apply_staged_ytdl(&self) -> Result<PathBuf> {
        let staged = self.ytdl_dir_staged();
        if !staged.join("yt-dlp.exe").is_file() {
            anyhow::bail!("No staged yt-dlp to apply at {staged:?}");
        }
        let live = self.ytdl_dir();
        let prev = self.ytdl_dir_previous();

        let had_live = live.exists();
        if had_live {
            let _ = std::fs::remove_dir_all(&prev);
            rename_patiently(&live, &prev).with_context(|| format!("Failed moving {live:?} aside to {prev:?}"))?;
        }
        if let Err(e) = rename_patiently(&staged, &live) {
            // Put the old build back rather than leaving the station with no
            // downloader at all.
            if had_live {
                let _ = rename_patiently(&prev, &live);
            }
            return Err(e).with_context(|| format!("Failed installing {staged:?} as {live:?}"));
        }
        info!("yt-dlp updated; previous build kept at {:?}", prev);
        Ok(self.ytdl_live())
    }

    /// Put the previous build back. The admin panel's "Rollback" after a bad
    /// release, and the nightly self-check's comparison. Without a previous
    /// folder build, the legacy one-file exe is the previous build.
    pub fn rollback_ytdl(&self) -> Result<PathBuf> {
        let prev = self.ytdl_dir_previous();
        let live = self.ytdl_dir();
        let staged = self.ytdl_dir_staged();
        if prev.join("yt-dlp.exe").is_file() {
            // The build being rolled back becomes the new `.prev`, so a
            // rollback can be undone.
            if live.exists() {
                let _ = std::fs::remove_dir_all(&staged);
                rename_patiently(&live, &staged)?;
            }
            rename_patiently(&prev, &live)?;
            if staged.exists() {
                let _ = rename_patiently(&staged, &prev);
            }
        } else if live.exists() && self.ytdl_legacy().is_file() {
            // Back to the one-file exe: the folder build steps aside as
            // `.prev`, and stepping forward again is another rollback.
            rename_patiently(&live, &prev)?;
        } else {
            anyhow::bail!("No previous yt-dlp to roll back to at {prev:?}");
        }
        info!("Rolled yt-dlp back to the previous build");
        Ok(self.ytdl_live())
    }

    /// Stage, then apply immediately.
    ///
    /// Kept for the manual "update now" button, where the operator is watching
    /// and has chosen the moment. The nightly path stages and applies
    /// separately so it can wait for downloads to finish.
    pub async fn download_ytdl(&self, channel: &str) -> Result<PathBuf> {
        self.stage_ytdl(channel).await?;
        self.apply_staged_ytdl()
    }

    // ------------------------------------------------------------------
    // Deno: the JavaScript runtime yt-dlp needs for YouTube (plan P6.8)
    // ------------------------------------------------------------------
    //
    // Since late 2025 yt-dlp solves YouTube's JavaScript challenge with an
    // external runtime. Without one it falls back to YouTube clients that
    // are deprecated, and downloads fail at random with HTTP 403 (two jobs
    // on 2026-10-06). yt-dlp.exe bundles the solver; only the runtime is
    // missing, and it is fetched like yt-dlp: verified, staged, previous
    // build kept.

    pub fn deno_live(&self) -> PathBuf {
        self.bin_dir.join("deno.exe")
    }
    pub fn deno_previous(&self) -> PathBuf {
        self.bin_dir.join("deno.exe.prev")
    }

    /// `deno --version`'s first line ("deno 2.9.7 (stable, …)") reduced to
    /// "2.9.7"; `None` when there is no working deno.
    pub fn deno_version(&self) -> Option<String> {
        let live = self.deno_live();
        if !live.is_file() {
            return None;
        }
        let out = self.get_tool_version(&live, "deno", "--version");
        parse_deno_version(&out)
    }

    /// The newest Deno release tag ("v2.9.7").
    pub async fn latest_deno_tag(&self) -> Result<String> {
        let client = reqwest::Client::builder()
            .user_agent("OmniDownloader-Rust/1.0")
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let resp = client
            .get("https://api.github.com/repos/denoland/deno/releases/latest")
            .send()
            .await
            .context("Deno release lookup failed")?
            .error_for_status()
            .context("Deno release lookup failed")?
            .json::<serde_json::Value>()
            .await?;
        resp.get("tag_name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("the Deno release has no tag"))
    }

    /// Download and put in place the latest Deno: [`Self::stage_deno`] then
    /// [`Self::apply_staged_deno`]. For start-up, where nothing is running; a
    /// caller with jobs in flight stages first and pauses only around the apply.
    pub async fn install_deno(&self) -> Result<String> {
        let version = self.stage_deno().await?;
        self.apply_staged_deno(&version)?;
        Ok(version)
    }

    fn deno_staged(&self) -> PathBuf {
        self.bin_dir.join("deno.exe.staged")
    }

    /// Throw away a staged Deno that will not be applied.
    pub fn discard_staged_deno(&self) {
        let _ = std::fs::remove_file(self.deno_staged());
    }

    /// Download the latest Deno, check it against the release's SHA-256 and
    /// prove it runs, leaving it staged as `deno.exe.staged`. Returns its version.
    pub async fn stage_deno(&self) -> Result<String> {
        const ZIP: &str = "deno-x86_64-pc-windows-msvc.zip";
        let base = "https://github.com/denoland/deno/releases/latest/download";
        let client = reqwest::Client::builder()
            .user_agent("OmniDownloader-Rust/1.0")
            .timeout(std::time::Duration::from_secs(600))
            .build()?;

        let sums = client
            .get(format!("{base}/{ZIP}.sha256sum"))
            .send()
            .await
            .context("Failed fetching the Deno checksum")?
            .error_for_status()
            .context("Deno checksum request failed")?
            .text()
            .await?;
        let expected = sha256_in_text(&sums)
            .ok_or_else(|| anyhow::anyhow!("the Deno checksum file has no SHA-256; refusing an unverified binary"))?;

        info!("Downloading Deno from {base}/{ZIP}");
        let bytes = client
            .get(format!("{base}/{ZIP}"))
            .send()
            .await
            .context("Failed downloading Deno")?
            .error_for_status()
            .context("Deno download failed")?
            .bytes()
            .await?;
        let actual = sha256_hex(&bytes);
        if actual != expected {
            anyhow::bail!(
                "Deno failed checksum verification (expected {expected}, got {actual}, {} bytes). Nothing was installed.",
                bytes.len()
            );
        }

        let exe = extract_from_zip(&bytes, "deno.exe")?;
        let staged = self.deno_staged();
        std::fs::write(&staged, &exe).with_context(|| format!("Failed writing {staged:?}"))?;
        let version = parse_deno_version(&self.get_tool_version(&staged, "deno", "--version"));
        let Some(version) = version else {
            let _ = std::fs::remove_file(&staged);
            anyhow::bail!("the downloaded Deno does not run on this machine; nothing was installed");
        };
        info!("Staged a verified Deno {version} (sha256 {})", &actual[..16]);
        Ok(version)
    }

    /// Swap the staged Deno in, the old one kept as `.prev`. The caller holds
    /// the swap guard. Renames only; no download.
    pub fn apply_staged_deno(&self, version: &str) -> Result<()> {
        let staged = self.deno_staged();
        if !staged.is_file() {
            anyhow::bail!("No staged Deno to apply at {staged:?}");
        }
        let live = self.deno_live();
        let prev = self.deno_previous();
        if live.exists() {
            let _ = std::fs::remove_file(&prev);
            std::fs::rename(&live, &prev).with_context(|| format!("Failed moving {live:?} aside"))?;
        }
        if let Err(e) = std::fs::rename(&staged, &live) {
            if prev.exists() {
                let _ = std::fs::rename(&prev, &live);
            }
            return Err(e).with_context(|| format!("Failed installing Deno as {live:?}"));
        }
        info!("Deno {version} installed at {live:?}");
        Ok(())
    }

    /// Put the previous Deno back after an update that made things worse.
    pub fn rollback_deno(&self) -> Result<()> {
        let prev = self.deno_previous();
        if !prev.exists() {
            anyhow::bail!("No previous Deno to roll back to");
        }
        let live = self.deno_live();
        let _ = std::fs::remove_file(&live);
        std::fs::rename(&prev, &live)?;
        info!("Rolled Deno back to the previous build");
        Ok(())
    }
}

/// "deno 2.9.7 (stable, release, x86_64-pc-windows-msvc)" → "2.9.7".
pub fn parse_deno_version(output: &str) -> Option<String> {
    let line = output.lines().find(|l| l.trim_start().starts_with("deno "))?;
    let v = line.trim_start().strip_prefix("deno ")?.split_whitespace().next()?;
    (v.chars().next()?.is_ascii_digit()).then(|| v.to_string())
}

/// The SHA-256 in a checksum file of any common shape: coreutils
/// (`<hex>  name`) or PowerShell's `Get-FileHash` table (`Hash : <HEX>`),
/// which is what Deno publishes for Windows.
pub fn sha256_in_text(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|t| t.len() == 64)
        .map(|t| t.to_ascii_lowercase())
}

/// One file out of a zip held in memory.
fn extract_from_zip(bytes: &[u8], name: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("the Deno download is not a zip file")?;
    let mut file = archive
        .by_name(name)
        .with_context(|| format!("the Deno download has no {name}"))?;
    let mut out = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut out)?;
    Ok(out)
}

/// The release asset of yt-dlp's one-folder Windows build.
pub const YTDL_ZIP: &str = "yt-dlp_win.zip";
/// The verified zip's hash, kept inside the folder it unpacked into.
const SHA_MARKER: &str = ".sha256";

/// The yt-dlp in `bin_dir`: the folder build when installed, else the
/// legacy one-file exe (which may not exist either).
pub fn ytdl_exe_in(bin_dir: &Path) -> PathBuf {
    let folder = bin_dir.join("yt-dlp").join("yt-dlp.exe");
    if folder.is_file() {
        folder
    } else {
        bin_dir.join("yt-dlp.exe")
    }
}

/// Unpack every file of a zip under `dest`, refusing entries that would
/// land outside it.
fn extract_zip_into(bytes: &[u8], dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("the yt-dlp download is not a zip file")?;
    std::fs::create_dir_all(dest).with_context(|| format!("Failed creating {dest:?}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            anyhow::bail!("the yt-dlp download has an entry outside its folder: {}", entry.name());
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(&out).with_context(|| format!("Failed creating {out:?}"))?;
        std::io::copy(&mut entry, &mut f).with_context(|| format!("Failed writing {out:?}"))?;
    }
    Ok(())
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// `rename`, retried briefly: right after a folder is unpacked, or right
/// after its exe exits, an antivirus scan can hold a handle for a moment
/// and Windows refuses to move the folder.
fn rename_patiently(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..5 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(200 * (attempt + 1)));
    }
    Err(last.expect("five attempts"))
}

/// A verified, staged tool binary.
#[derive(Debug, Clone)]
pub struct StagedTool {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

/// Find `filename`'s hash in a `SHA2-256SUMS` file.
///
/// Lines are `<hex>  <name>`, with two spaces in the coreutils format, but the
/// separator is not worth being strict about — matching on whitespace tolerates
/// both that and the single-space variant some releases use.
pub fn expected_sha256(sums: &str, filename: &str) -> Option<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next().unwrap_or("").trim_start_matches('*');
        if name.eq_ignore_ascii_case(filename) && hash.len() == 64 {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
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

// ----------------------------------------------------------------------
// Pinned installs: ffmpeg, ffprobe, bmxtranswrap (plan P9.3)
// ----------------------------------------------------------------------
//
// Fetched only at the build in `toolpins::PINS`, checked against its SHA-256,
// swapped in under the update gate with the previous file kept as `.prev`.
// Never run by a scheduled task: the operator presses «Install» or «Repair».

/// Drain budget for a swap: longer than any sane clip, as the nightly yt-dlp update.
pub const SWAP_DRAIN: Duration = Duration::from_secs(600);
/// Version probes of an installed tool.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// No single pinned member is larger than this; a lying zip header cannot fill the disk.
const MEMBER_CAP: u64 = 600 * 1024 * 1024;

pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Where a pinned archive comes from. The real one is HTTP; tests hand over bytes.
pub trait Fetcher: Send + Sync {
    /// Stream the body of `url` into `sink`, chunk by chunk. An `Err` from the
    /// sink stops the transfer and is returned.
    fn fetch<'a>(&'a self, url: &'a str, sink: &'a mut (dyn FnMut(&[u8]) -> Result<()> + Send)) -> FetchFuture<'a>;
}

/// `reqwest`, streaming.
pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn fetch<'a>(&'a self, url: &'a str, sink: &'a mut (dyn FnMut(&[u8]) -> Result<()> + Send)) -> FetchFuture<'a> {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .user_agent("OmniDownloader-Rust/1.0")
                .timeout(Duration::from_secs(1800))
                .build()?;
            // `without_url`: a redirect target carries a signed query string.
            let mut resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| e.without_url())
                .context("the download could not start")?
                .error_for_status()
                .map_err(|e| e.without_url())
                .context("the download was refused")?;
            while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url()).context("the download was interrupted")? {
                sink(&chunk)?;
            }
            Ok(())
        })
    }
}

/// An install in progress, for the panel.
#[derive(Debug, Clone, Serialize)]
pub struct InstallRunning {
    pub tool: String,
    pub stage: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// How the last install of a tool ended.
#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    pub ok: bool,
    pub message: String,
    pub needs_restart: bool,
}

#[derive(Debug, Clone)]
struct FileFacts {
    len: u64,
    modified: Option<std::time::SystemTime>,
    sha256: Option<String>,
    version: Option<Option<String>>,
}

#[derive(Debug, Default)]
struct InstallerInner {
    running: Option<InstallRunning>,
    last: HashMap<String, InstallResult>,
    facts: HashMap<PathBuf, FileFacts>,
}

/// Shared by the admin routes: which install runs now, how each ended, and a
/// cache of the hash and version of the installed files (the panel polls, and
/// ffmpeg is 100 MB).
#[derive(Debug, Clone, Default)]
pub struct ToolInstaller {
    inner: Arc<Mutex<InstallerInner>>,
}

impl ToolInstaller {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InstallerInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Claim the single install slot. `Err` names the tool that holds it.
    pub fn try_begin(&self, tool: &str) -> std::result::Result<InstallGuard, String> {
        let mut g = self.lock();
        if let Some(r) = &g.running {
            return Err(r.tool.clone());
        }
        g.running = Some(InstallRunning { tool: tool.to_string(), stage: "starting".into(), bytes_done: 0, bytes_total: 0 });
        Ok(InstallGuard { installer: self.clone(), tool: tool.to_string(), finished: false })
    }

    pub fn running(&self) -> Option<InstallRunning> {
        self.lock().running.clone()
    }

    pub fn last_result(&self, tool: &str) -> Option<InstallResult> {
        self.lock().last.get(tool).cloned()
    }

    pub fn set_stage(&self, stage: &str) {
        if let Some(r) = self.lock().running.as_mut() {
            r.stage = stage.to_string();
        }
    }

    pub fn set_bytes(&self, done: u64, total: u64) {
        if let Some(r) = self.lock().running.as_mut() {
            r.bytes_done = done;
            r.bytes_total = total;
        }
    }

    fn stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
        let m = std::fs::metadata(path).ok()?;
        m.is_file().then(|| (m.len(), m.modified().ok()))
    }

    /// SHA-256 of a file, remembered until its size or time changes. Blocking.
    pub fn sha256_of(&self, path: &Path) -> Option<String> {
        use sha2::{Digest, Sha256};
        let (len, modified) = Self::stamp(path)?;
        if let Some(f) = self.lock().facts.get(path) {
            if f.len == len && f.modified == modified {
                if let Some(h) = &f.sha256 {
                    return Some(h.clone());
                }
            }
        }
        let mut file = std::fs::File::open(path).ok()?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher).ok()?;
        let hex = hex_of(&hasher.finalize());
        let mut g = self.lock();
        let entry = g.facts.entry(path.to_path_buf()).or_insert(FileFacts { len, modified, sha256: None, version: None });
        if entry.len != len || entry.modified != modified {
            *entry = FileFacts { len, modified, sha256: None, version: None };
        }
        entry.sha256 = Some(hex.clone());
        Some(hex)
    }

    fn cached_version(&self, path: &Path) -> Option<Option<String>> {
        let (len, modified) = Self::stamp(path)?;
        let g = self.lock();
        let f = g.facts.get(path)?;
        if f.len == len && f.modified == modified {
            f.version.clone()
        } else {
            None
        }
    }

    fn remember_version(&self, path: &Path, version: Option<String>) {
        let Some((len, modified)) = Self::stamp(path) else { return };
        let mut g = self.lock();
        let entry = g.facts.entry(path.to_path_buf()).or_insert(FileFacts { len, modified, sha256: None, version: None });
        if entry.len != len || entry.modified != modified {
            *entry = FileFacts { len, modified, sha256: None, version: None };
        }
        entry.version = Some(version);
    }
}

/// The install slot. Ends with [`InstallGuard::finish`]; dropped without it
/// (a panic), the slot is freed and the tool's result says it was interrupted.
pub struct InstallGuard {
    installer: ToolInstaller,
    tool: String,
    finished: bool,
}

impl InstallGuard {
    pub fn finish(mut self, result: InstallResult) {
        self.end(result);
    }

    fn end(&mut self, result: InstallResult) {
        self.finished = true;
        let mut g = self.installer.lock();
        g.running = None;
        g.last.insert(self.tool.clone(), result);
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.end(InstallResult { ok: false, message: "the install was interrupted".into(), needs_restart: false });
        }
    }
}

fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// What a pinned install did.
#[derive(Debug, Clone)]
pub struct PinnedOutcome {
    /// Every file already had its pinned hash; nothing was downloaded.
    pub already: bool,
    pub message: String,
}

/// One tool as the panel lists it.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub name: String,
    pub path: String,
    pub installed: bool,
    pub version: Option<String>,
    pub pinned_version: Option<String>,
    pub matches_pin: Option<bool>,
    /// `install` when missing, `repair` when it is not the pinned build or does not run.
    pub action: Option<&'static str>,
    /// `bin` or `config`: where the running file comes from.
    pub source: &'static str,
    /// Said when `config.json` points somewhere else than `bin/`.
    pub note: Option<String>,
}

/// The tools of the panel, in order, as `(name, file, version flag)`.
const TOOL_PROBES: &[(&str, &str, &str)] = &[
    ("yt-dlp", "yt-dlp.exe", "--version"),
    ("ffmpeg", "ffmpeg.exe", "-version"),
    ("ffprobe", "ffprobe.exe", "-version"),
    ("bmxtranswrap", "bmxtranswrap.exe", "-v"),
    ("deno", "deno.exe", "--version"),
];

/// The version a tool printed, `None` when the output has none.
pub fn parse_tool_version(name: &str, output: &str) -> Option<String> {
    match name {
        "deno" => parse_deno_version(output),
        "ffmpeg" | "ffprobe" => {
            let re = Regex::new(r"version\s+([^\s]+)").ok()?;
            re.captures(output).map(|c| c[1].to_string())
        }
        "bmxtranswrap" => {
            let re = Regex::new(r"v(\d+\.\d+\.\d+)").ok()?;
            re.captures(output).map(|c| c[1].to_string())
        }
        _ => output.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string),
    }
}

/// Run `exe args` for a version answer: stdout then stderr, `None` when it
/// could not run or timed out.
async fn probe_output(exe: &Path, args: &[&str], cwd: &Path) -> Option<String> {
    let opts = crate::process::RunOpts::new(PROBE_TIMEOUT).capture_stdout().with_current_dir(cwd);
    let out = crate::process::run(exe, args.iter().copied(), opts).await.ok()?;
    if out.timed_out || out.cancelled {
        return None;
    }
    Some(format!("{}\n{}", out.stdout, out.stderr_tail))
}

fn probe_matches(pin: &ToolPin, output: &str) -> bool {
    if pin.probe_first_line {
        output.lines().map(str::trim).find(|l| !l.is_empty()).is_some_and(|l| l.contains(pin.probe_contains))
    } else {
        output.contains(pin.probe_contains)
    }
}

/// Entry names are matched on `/`, whichever separator the archive used.
fn entry_matches(name: &str, suffix: &str) -> bool {
    let name = name.replace('\\', "/");
    name == suffix || name.ends_with(&format!("/{suffix}"))
}

impl DependencyManager {
    /// Point the VC++ runtime lookup somewhere else (tests).
    pub fn with_system_dir<P: AsRef<Path>>(mut self, dir: P) -> Self {
        self.system_dir = dir.as_ref().to_path_buf();
        self
    }

    /// Replace the version probe of a pinned install (tests: a fake exe cannot run).
    pub fn with_probe_hook(mut self, hook: ProbeHook) -> Self {
        self.probe_hook = Some(hook);
        self
    }

    fn staged_dir(&self, pin: &ToolPin) -> PathBuf {
        self.bin_dir.join(format!("{}.staged", pin.tool))
    }

    fn clone_paths(&self) -> DependencyManager {
        DependencyManager {
            bin_dir: self.bin_dir.clone(),
            system_dir: self.system_dir.clone(),
            probe_hook: self.probe_hook.clone(),
        }
    }

    /// Whether every file of the pin is installed with its pinned hash. Blocking.
    pub fn pin_is_current(&self, pin: &ToolPin, st: &ToolInstaller) -> bool {
        pin.members.iter().all(|m| st.sha256_of(&self.bin_dir.join(m.exe)).as_deref() == Some(m.sha256))
    }

    /// At start-up: finish a swap that was interrupted (power cut, kill).
    ///
    /// The evidence is `bin/<tool>.staged/`, which every normal exit removes. A staged
    /// member is re-hashed and, only if it equals the pin, moved forward to live (the file
    /// it replaces becomes `.prev`). A staged member with another hash is discarded and the
    /// tool stays as it is, missing if it was. `.prev` is never put back: after a finished
    /// swap that someone deleted the new file from, the old build must not come back on
    /// air unannounced. Returns the members completed.
    pub fn recover_interrupted_swaps(&self) -> Vec<String> {
        self.recover_interrupted_swaps_for(crate::toolpins::PINS)
    }

    fn recover_interrupted_swaps_for(&self, pins: &[ToolPin]) -> Vec<String> {
        let st = ToolInstaller::new();
        let mut completed = Vec::new();
        for pin in pins {
            let staged = self.staged_dir(pin);
            if !staged.is_dir() {
                continue;
            }
            for m in pin.members {
                let src = staged.join(m.exe);
                if !src.is_file() {
                    continue;
                }
                let live = self.bin_dir.join(m.exe);
                if st.sha256_of(&src).as_deref() != Some(m.sha256) {
                    tracing::error!("{} in the interrupted {} install is not the pinned build; discarded", m.exe, pin.tool);
                    continue;
                }
                if st.sha256_of(&live).as_deref() == Some(m.sha256) {
                    continue;
                }
                let prev = self.bin_dir.join(format!("{}.prev", m.exe));
                let moved_aside = live.exists();
                if moved_aside {
                    let _ = std::fs::remove_file(&prev);
                    if let Err(e) = rename_patiently(&live, &prev) {
                        tracing::error!("{} could not be moved aside to finish the interrupted swap: {e}", m.exe);
                        continue;
                    }
                }
                match rename_patiently(&src, &live) {
                    Ok(()) => {
                        tracing::warn!("{} was finished from an interrupted swap (staged file matched the pin)", m.exe);
                        completed.push(m.exe.to_string());
                    }
                    Err(e) => {
                        tracing::error!("{} could not be put in place: {e}", m.exe);
                        // Recovery must not leave a tool missing that was present: the
                        // file moved aside was live a moment ago, so it goes back.
                        if moved_aside {
                            if let Err(e) = rename_patiently(&prev, &live) {
                                tracing::error!("{} could not be put back either: {e}; it is in {}.prev", m.exe, m.exe);
                            }
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&staged);
        }
        completed
    }

    /// The «pinned tools» health line: amber while a pinned tool in `bin/` (or the file
    /// `config.json` points to instead) is missing or is not the pinned build. The hash
    /// cache keeps this cheap to repeat. Blocking.
    pub fn pinned_tools_check(&self, st: &ToolInstaller, overrides: &HashMap<String, PathBuf>) -> crate::health::Check {
        self.pinned_tools_check_for(crate::toolpins::PINS, st, overrides)
    }

    fn pinned_tools_check_for(&self, pins: &[ToolPin], st: &ToolInstaller, overrides: &HashMap<String, PathBuf>) -> crate::health::Check {
        let mut bad: Vec<String> = Vec::new();
        let mut names: Vec<&str> = Vec::new();
        for pin in pins {
            for m in pin.members {
                let tool = m.exe.trim_end_matches(".exe");
                names.push(tool);
                let over = overrides.get(tool);
                let path = over.cloned().unwrap_or_else(|| self.bin_dir.join(m.exe));
                if st.sha256_of(&path).as_deref() == Some(m.sha256) {
                    continue;
                }
                bad.push(match over {
                    None => format!("{tool} is not the pinned build: Admin \u{2192} Tools \u{2192} Repair"),
                    Some(p) => format!(
                        "{tool} is not the pinned build (set in config.json: {}; that file is what runs): Admin \u{2192} Tools \u{2192} Repair changes only the copy in bin/",
                        p.display()
                    ),
                });
            }
        }
        if bad.is_empty() {
            crate::health::Check::ok(format!("{} are the pinned builds", names.join(", ")))
        } else {
            crate::health::Check::degraded(bad.join("; "))
        }
    }

    /// The tools of the admin panel, with what each needs. `overrides` are the
    /// paths `config.json` sets, which the daemon runs instead of `bin/`.
    pub async fn tool_statuses(&self, st: &ToolInstaller, overrides: &HashMap<String, PathBuf>) -> Vec<ToolStatus> {
        let mut rows = Vec::new();
        for (name, file, flag) in TOOL_PROBES {
            let bin_path = if *name == "yt-dlp" { self.ytdl_live() } else { self.bin_dir.join(file) };
            let over = overrides.get(*name);
            let path = over.cloned().unwrap_or_else(|| bin_path.clone());
            let installed = path.is_file();
            let pin = crate::toolpins::pin_for(name);
            let want = pin.and_then(|p| crate::toolpins::member_for(p, name)).map(|m| m.sha256);
            let mut version = None;
            if installed {
                version = match st.cached_version(&path) {
                    Some(v) => v,
                    None => {
                        let out = probe_output(&path, &[flag], &self.bin_dir).await;
                        let v = out.and_then(|o| parse_tool_version(name, &o));
                        st.remember_version(&path, v.clone());
                        v
                    }
                };
            }
            let hash_matches = |p: PathBuf| {
                let st2 = st.clone();
                async move { tokio::task::spawn_blocking(move || st2.sha256_of(&p)).await.ok().flatten() }
            };
            let matches_pin = match (pin, installed) {
                (Some(_), true) => Some(want.is_some() && hash_matches(path.clone()).await.as_deref() == want),
                (Some(_), false) => Some(false),
                (None, _) => None,
            };
            let action = if over.is_some() {
                // The override is what runs; only the copy in bin/ can be installed or repaired.
                if !bin_path.is_file() {
                    (pin.is_some() && matches_pin != Some(true)).then_some("install")
                } else if pin.is_some() && hash_matches(bin_path.clone()).await.as_deref() != want {
                    Some("repair")
                } else {
                    None
                }
            } else if !installed {
                Some("install")
            } else if version.is_none() || matches_pin == Some(false) {
                Some("repair")
            } else {
                None
            };
            rows.push(ToolStatus {
                name: name.to_string(),
                path: if installed { path.to_string_lossy().to_string() } else { "Missing".into() },
                installed,
                version,
                pinned_version: pin.map(|p| p.version.to_string()),
                matches_pin,
                action,
                source: if over.is_some() { "config" } else { "bin" },
                note: over.map(|p| {
                    format!(
                        "set in config.json: {}. The service runs this file; Install and Repair change only the copy in bin/.",
                        p.display()
                    )
                }),
            });
        }
        rows
    }

    /// The VC++ runtime DLLs that are in neither `bin/` nor System32.
    fn missing_vcrt(&self) -> Vec<&'static str> {
        crate::toolpins::VCRT_DLLS
            .iter()
            .copied()
            .filter(|d| !self.bin_dir.join(d).is_file() && !self.system_dir.join(d).is_file())
            .collect()
    }

    fn vcrt_missing_error(&self, pin: &ToolPin, missing: &[&str]) -> anyhow::Error {
        anyhow::anyhow!(
            "{} needs the Microsoft Visual C++ runtime, which is missing ({}): install the VC++ 2015-2022 x64 redistributable, then try again",
            pin.tool,
            missing.join(", ")
        )
    }

    /// The runtime DLLs next to the tool: ones only in System32 are copied into
    /// `bin/` (never over a file already there); ones nowhere are an error.
    /// Returns the names copied. Blocking.
    fn ensure_vcrt(&self, pin: &ToolPin) -> Result<Vec<&'static str>> {
        if !pin.needs_vcrt {
            return Ok(Vec::new());
        }
        let missing = self.missing_vcrt();
        if !missing.is_empty() {
            return Err(self.vcrt_missing_error(pin, &missing));
        }
        let mut copied = Vec::new();
        for dll in crate::toolpins::VCRT_DLLS {
            let target = self.bin_dir.join(dll);
            if !target.exists() {
                std::fs::copy(self.system_dir.join(dll), &target)
                    .with_context(|| format!("Failed copying {dll} into {:?}", self.bin_dir))?;
                copied.push(*dll);
            }
        }
        Ok(copied)
    }

    fn probing(&self, pin: &ToolPin) -> bool {
        self.probe_hook.is_some() || !pin.probe_contains.is_empty()
    }

    /// Install `pin`: download, verify, stage, swap under the gate, check again.
    ///
    /// Every exit path removes the staged folder, and any failure before the
    /// swap leaves the live files exactly as they were.
    pub async fn install_pinned(
        &self,
        pin: &ToolPin,
        fetcher: &dyn Fetcher,
        gate: &UpdateGate,
        drain: Duration,
        st: &ToolInstaller,
    ) -> Result<PinnedOutcome> {
        st.set_stage("checking");
        let (me, pin2, st2) = (self.clone_paths(), pin.clone(), st.clone());
        let current = tokio::task::spawn_blocking(move || me.pin_is_current(&pin2, &st2)).await?;
        if current {
            // The files are the pinned build, so a download would change nothing. What can
            // still be wrong is the runtime next to them or the file not running: say which.
            let (me, pin2) = (self.clone_paths(), pin.clone());
            let copied = tokio::task::spawn_blocking(move || me.ensure_vcrt(&pin2)).await??;
            if self.probing(pin) {
                self.check_probe(pin, &self.bin_dir.join(pin.probe_exe), "the installed")
                    .await
                    .context("the file has the pinned hash, so downloading it again would not help")?;
            }
            let mut message = format!("{} is already at the pinned version {}", pin.tool, pin.version);
            if !copied.is_empty() {
                message.push_str(&format!("; the VC++ runtime ({}) was copied from System32 into bin/", copied.join(", ")));
            }
            return Ok(PinnedOutcome { already: true, message });
        }
        let staged = self.staged_dir(pin);
        let _ = std::fs::remove_dir_all(&staged);
        let result = self.install_staged(pin, fetcher, gate, drain, st, &staged).await;
        let _ = std::fs::remove_dir_all(&staged);
        result
    }

    async fn install_staged(
        &self,
        pin: &ToolPin,
        fetcher: &dyn Fetcher,
        gate: &UpdateGate,
        drain: Duration,
        st: &ToolInstaller,
        staged: &Path,
    ) -> Result<PinnedOutcome> {
        use sha2::{Digest, Sha256};
        if pin.needs_vcrt {
            let missing = self.missing_vcrt();
            if !missing.is_empty() {
                return Err(self.vcrt_missing_error(pin, &missing));
            }
        }
        std::fs::create_dir_all(staged).with_context(|| format!("Failed creating {staged:?}"))?;

        // 1. Download, capped at the pinned size.
        st.set_stage("downloading");
        st.set_bytes(0, pin.size);
        let zip_path = staged.join("archive.zip");
        let mut file = std::fs::File::create(&zip_path).with_context(|| format!("Failed creating {zip_path:?}"))?;
        let mut done: u64 = 0;
        let (size, tool) = (pin.size, pin.tool);
        let st_sink = st.clone();
        let mut sink = move |chunk: &[u8]| -> Result<()> {
            use std::io::Write;
            done += chunk.len() as u64;
            if done > size {
                anyhow::bail!("the {tool} download is larger than the pinned {size} bytes; stopped, nothing changed");
            }
            file.write_all(chunk).context("Failed writing the download")?;
            st_sink.set_bytes(done, size);
            Ok(())
        };
        info!("Downloading pinned {} {} from {}", pin.tool, pin.version, host_and_path(pin.url));
        fetcher.fetch(pin.url, &mut sink).await?;
        drop(sink);
        let got_len = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
        if got_len != pin.size {
            anyhow::bail!("the {} download is {got_len} bytes, the pinned build is {}; nothing changed", pin.tool, pin.size);
        }

        // 2. Verify the archive when its hash is pinned, then take only the pinned members.
        st.set_stage("verifying");
        let (zip2, pin2, staged2) = (zip_path.clone(), pin.clone(), staged.to_path_buf());
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(want) = pin2.archive_sha256 {
                let mut f = std::fs::File::open(&zip2)?;
                let mut h = Sha256::new();
                std::io::copy(&mut f, &mut h)?;
                let got = hex_of(&h.finalize());
                if got != want {
                    anyhow::bail!("the {} archive failed its checksum (expected {want}, got {got}); nothing changed", pin2.tool);
                }
            }
            extract_pinned_members(&zip2, &pin2, &staged2)
        })
        .await
        .context("unpack task")??;
        let _ = std::fs::remove_file(&zip_path);

        // 3. Prove the staged build runs, and is the version pinned.
        if self.probing(pin) {
            st.set_stage("testing");
            self.check_probe(pin, &staged.join(pin.probe_exe), "the downloaded").await?;
        }

        // 4. Swap, with downloads stopped and no other swapper at work.
        st.set_stage("waiting for jobs");
        let Some(_swap) = gate.begin_swap(drain).await else {
            anyhow::bail!("jobs still running; try again");
        };
        st.set_stage("installing");
        self.swap_in(pin, staged).await
    }

    async fn check_probe(&self, pin: &ToolPin, exe: &Path, what: &str) -> Result<()> {
        if let Some(hook) = &self.probe_hook {
            return if hook(exe) { Ok(()) } else { Err(anyhow::anyhow!("{what} {} does not pass its check; nothing changed", pin.tool)) };
        }
        match probe_output(exe, pin.probe_args, &self.bin_dir).await {
            Some(o) if probe_matches(pin, &o) => Ok(()),
            Some(_) => anyhow::bail!("{what} {} does not report {}; nothing changed", pin.tool, pin.probe_contains),
            None => anyhow::bail!("{what} {} does not run on this machine; nothing changed", pin.tool),
        }
    }

    /// Move the staged files in, the old ones to `.prev`; check; undo on failure.
    async fn swap_in(&self, pin: &ToolPin, staged: &Path) -> Result<PinnedOutcome> {
        let (me, pin2, staged2) = (self.clone_paths(), pin.clone(), staged.to_path_buf());
        let moved = tokio::task::spawn_blocking(move || me.swap_files(&pin2, &staged2))
            .await
            .context("swap task")??;
        if self.probing(pin) {
            if let Err(e) = self.check_probe(pin, &self.bin_dir.join(pin.probe_exe), "the installed").await {
                let me = self.clone_paths();
                let undone = tokio::task::spawn_blocking(move || me.undo_swap(&moved)).await.context("rollback task")?;
                return Err(match undone {
                    Ok(()) => anyhow::anyhow!("{e:#}; the previous files were put back"),
                    Err(still) => anyhow::anyhow!("{e:#}; {}", rollback_failed(&still)),
                });
            }
        }
        info!("{} {} installed in {:?}; previous files kept as .prev", pin.tool, pin.version, self.bin_dir);
        Ok(PinnedOutcome {
            already: false,
            message: format!("{} {} installed; the previous files are kept as .prev", pin.tool, pin.version),
        })
    }

    /// Blocking: renames retry with sleeps.
    fn swap_files(&self, pin: &ToolPin, staged: &Path) -> Result<Vec<Swapped>> {
        // Runtime DLLs first, and only ever added: a file already in bin/ stays.
        self.ensure_vcrt(pin)?;
        let mut done: Vec<Swapped> = Vec::new();
        for m in pin.members {
            let live = self.bin_dir.join(m.exe);
            let prev = self.bin_dir.join(format!("{}.prev", m.exe));
            let src = staged.join(m.exe);
            let had_live = live.exists();
            if had_live {
                let _ = std::fs::remove_file(&prev);
                if let Err(e) = rename_patiently(&live, &prev) {
                    let err = anyhow::Error::new(e).context(format!("Failed moving {live:?} aside"));
                    return Err(self.fail_and_undo(&done, err));
                }
            }
            // Recorded before the second rename, so a failure of it is undone with the rest.
            done.push(Swapped { live: live.clone(), prev, had_live });
            if let Err(e) = rename_patiently(&src, &live) {
                let err = anyhow::Error::new(e).context(format!("Failed installing {live:?}"));
                return Err(self.fail_and_undo(&done, err));
            }
        }
        Ok(done)
    }

    fn fail_and_undo(&self, done: &[Swapped], err: anyhow::Error) -> anyhow::Error {
        match self.undo_swap(done) {
            Ok(()) => anyhow::anyhow!("{err:#}; the previous files were put back"),
            Err(still) => anyhow::anyhow!("{err:#}; {}", rollback_failed(&still)),
        }
    }

    /// Put every moved file back. `Err` says which `.prev` files could not be
    /// restored and are still there, and whether the live file is gone. Blocking.
    fn undo_swap(&self, done: &[Swapped]) -> std::result::Result<(), Vec<RollbackFailure>> {
        let mut failed = Vec::new();
        for s in done.iter().rev() {
            if !s.had_live {
                // The member did not exist before the swap.
                match std::fs::remove_file(&s.live) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => failed.push(RollbackFailure { prev: file_name_of(&s.prev), live: file_name_of(&s.live), live_missing: false }),
                }
                continue;
            }
            // Straight over the live file (a rename replaces it): there is never a moment with no
            // live file. Remove-then-rename only when that is refused.
            if std::fs::rename(&s.prev, &s.live).is_ok() {
                continue;
            }
            let removed = match std::fs::remove_file(&s.live) {
                Ok(()) => true,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            };
            if removed && rename_patiently(&s.prev, &s.live).is_ok() {
                continue;
            }
            failed.push(RollbackFailure { prev: file_name_of(&s.prev), live: file_name_of(&s.live), live_missing: !s.live.exists() });
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed)
        }
    }
}

/// One file a rollback could not restore.
#[derive(Debug, PartialEq, Eq)]
struct RollbackFailure {
    prev: String,
    live: String,
    live_missing: bool,
}

fn file_name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn rollback_failed(failed: &[RollbackFailure]) -> String {
    let parts: Vec<String> = failed
        .iter()
        .map(|f| {
            if f.live_missing {
                format!("{} is now missing; {} is still there", f.live, f.prev)
            } else {
                format!("{} is still there", f.prev)
            }
        })
        .collect();
    format!("rollback failed; {}", parts.join("; "))
}

#[derive(Debug)]
struct Swapped {
    live: PathBuf,
    prev: PathBuf,
    had_live: bool,
}

/// `host/path` of a URL, never its query.
fn host_and_path(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['?', '#']).next().unwrap_or(rest).to_string()
}

/// Write exactly the pinned members of the archive into `dest`, each under its
/// pinned name (never the entry's own path), and check each against its hash.
/// Nothing else in the archive is read out.
fn extract_pinned_members(zip_path: &Path, pin: &ToolPin, dest: &Path) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let file = std::fs::File::open(zip_path).with_context(|| format!("Failed opening {zip_path:?}"))?;
    let mut archive = zip::ZipArchive::new(file).with_context(|| format!("the {} download is not a zip file", pin.tool))?;
    for m in pin.members {
        let mut found: Option<usize> = None;
        for i in 0..archive.len() {
            let entry = archive.by_index(i)?;
            if entry.is_dir() || !entry_matches(entry.name(), m.entry_suffix) {
                continue;
            }
            if found.is_some() {
                anyhow::bail!("the {} archive has more than one {}; nothing changed", pin.tool, m.exe);
            }
            found = Some(i);
        }
        let Some(i) = found else {
            anyhow::bail!("the {} archive has no {}; nothing changed", pin.tool, m.exe);
        };
        let entry = archive.by_index(i)?;
        let out_path = dest.join(m.exe);
        let mut out = std::fs::File::create(&out_path).with_context(|| format!("Failed creating {out_path:?}"))?;
        let mut hasher = Sha256::new();
        let mut limited = entry.take(MEMBER_CAP + 1);
        let mut buf = [0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = limited.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MEMBER_CAP {
                anyhow::bail!("{} in the {} archive is implausibly large; nothing changed", m.exe, pin.tool);
            }
            hasher.update(&buf[..n]);
            std::io::Write::write_all(&mut out, &buf[..n])?;
        }
        // On disk before the swap: a power cut after it must not leave an empty exe.
        out.sync_all().with_context(|| format!("Failed flushing {out_path:?}"))?;
        let got = hex_of(&hasher.finalize());
        if got != m.sha256 {
            anyhow::bail!(
                "{} in the {} release is not the binary that was validated for air (expected {}, got {got}); refused, nothing changed",
                m.exe,
                pin.tool,
                m.sha256
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn deno_checksums_and_versions_read_in_the_shapes_deno_publishes() {
        // What deno-x86_64-pc-windows-msvc.zip.sha256sum contains (v2.9.7).
        let ps = "\r\nAlgorithm : SHA256\r\nHash      : A0C3101B4158D1DFB7D6A78A7BF0F3DE80C96BB423C152BEEC8BEB22786F2238\r\nPath      : C:\\a\\deno\\deno-x86_64-pc-windows-msvc.zip\r\n";
        assert_eq!(
            sha256_in_text(ps).as_deref(),
            Some("a0c3101b4158d1dfb7d6a78a7bf0f3de80c96bb423c152beec8beb22786f2238")
        );
        assert_eq!(
            sha256_in_text("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  deno.zip").map(|s| s.len()),
            Some(64)
        );
        assert_eq!(sha256_in_text("no hash here, only deadbeef"), None);

        assert_eq!(
            parse_deno_version("deno 2.9.7 (stable, release, x86_64-pc-windows-msvc)\nv8 14.1\ntypescript 5.9").as_deref(),
            Some("2.9.7")
        );
        assert_eq!(parse_deno_version("Error running binary"), None);
    }

    #[test]
    fn deno_is_taken_out_of_its_release_zip() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file("deno.exe", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(b"MZ fake exe").unwrap();
            z.finish().unwrap();
        }
        assert_eq!(extract_from_zip(buf.get_ref(), "deno.exe").unwrap(), b"MZ fake exe");
        assert!(extract_from_zip(buf.get_ref(), "other.exe").is_err());
        assert!(extract_from_zip(b"not a zip", "deno.exe").is_err());
    }

    #[test]
    fn a_checksum_file_is_parsed_the_way_github_writes_it() {
        // The real SHA2-256SUMS has many entries; only yt-dlp.exe matters, and
        // picking the wrong line would compare a good download against another
        // artifact's hash and reject every update forever.
        let sums = "\
d2f3a1b0c9e8d7c6b5a4938271605f4e3d2c1b0a9f8e7d6c5b4a39281706f5e4  yt-dlp
aa11bb22cc33dd44ee55ff66007788990011223344556677889900aabbccddee  yt-dlp.exe
ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff  yt-dlp_macos
";
        assert_eq!(
            expected_sha256(sums, "yt-dlp.exe").as_deref(),
            Some("aa11bb22cc33dd44ee55ff66007788990011223344556677889900aabbccddee")
        );
        assert_eq!(expected_sha256(sums, "yt-dlp_linux"), None);
    }

    #[test]
    fn checksum_parsing_tolerates_the_binary_marker_and_odd_spacing() {
        assert_eq!(
            expected_sha256(
                "AA11BB22CC33DD44EE55FF66007788990011223344556677889900AABBCCDDEE *yt-dlp.exe",
                "yt-dlp.exe"
            )
            .as_deref(),
            Some("aa11bb22cc33dd44ee55ff66007788990011223344556677889900aabbccddee")
        );
    }

    #[test]
    fn a_truncated_or_junk_checksum_line_is_not_accepted() {
        // An HTML error page saved as SHA2-256SUMS (captive portal, proxy) must
        // not parse into something that looks like a hash.
        assert_eq!(expected_sha256("<html><body>403</body></html>", "yt-dlp.exe"), None);
        assert_eq!(expected_sha256("deadbeef  yt-dlp.exe", "yt-dlp.exe"), None);
        assert_eq!(expected_sha256("", "yt-dlp.exe"), None);
    }

    #[test]
    fn sha256_matches_the_known_digest_of_the_empty_input() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// A folder build as `stage_ytdl` leaves it: exe, a DLL, the hash marker.
    fn stage_folder(mgr: &DependencyManager, content: &[u8], sha: &str) {
        let d = mgr.ytdl_dir_staged();
        std::fs::create_dir_all(d.join("_internal")).unwrap();
        std::fs::write(d.join("yt-dlp.exe"), content).unwrap();
        std::fs::write(d.join("_internal").join("python312.dll"), content).unwrap();
        std::fs::write(d.join(SHA_MARKER), sha).unwrap();
    }

    #[test]
    fn applying_a_staged_build_keeps_the_previous_one_for_rollback() {
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());

        stage_folder(&mgr, b"old build", "aaa");
        mgr.apply_staged_ytdl().unwrap();
        stage_folder(&mgr, b"new build", "bbb");
        let live = mgr.apply_staged_ytdl().unwrap();

        assert_eq!(live, dir.path().join("yt-dlp").join("yt-dlp.exe"));
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"new build");
        assert_eq!(mgr.ytdl_live_sha256().as_deref(), Some("bbb"));
        assert_eq!(
            std::fs::read(mgr.ytdl_dir().join("_internal").join("python312.dll")).unwrap(),
            b"new build",
            "the whole folder moves, not just the exe"
        );
        assert!(!mgr.ytdl_staged().exists());

        // A yt-dlp release that breaks an extractor is a Tuesday. Rolling back
        // must not require finding the old build by hand -- and every caller
        // keeps the same path.
        assert_eq!(mgr.rollback_ytdl().unwrap(), live);
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"old build");
        assert_eq!(mgr.ytdl_live_sha256().as_deref(), Some("aaa"));
        // ...and the rollback itself is reversible.
        mgr.rollback_ytdl().unwrap();
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"new build");
    }

    #[test]
    fn applying_with_nothing_staged_is_an_error_not_a_silent_no_op() {
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());
        stage_folder(&mgr, b"old build", "aaa");
        mgr.apply_staged_ytdl().unwrap();

        assert!(mgr.apply_staged_ytdl().is_err());
        // The working build is untouched.
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"old build");
    }

    #[test]
    fn rollback_without_a_previous_build_refuses_rather_than_removing_the_live_one() {
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());
        stage_folder(&mgr, b"only build", "aaa");
        mgr.apply_staged_ytdl().unwrap();

        assert!(mgr.rollback_ytdl().is_err());
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"only build");

        // The legacy one-file exe alone, never a folder build: nothing to go back to either.
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());
        std::fs::write(mgr.ytdl_legacy(), b"one-file").unwrap();
        assert!(mgr.rollback_ytdl().is_err());
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"one-file");
    }

    #[test]
    fn the_folder_build_replaces_the_one_file_exe_and_can_step_back_to_it() {
        // P2.10: the one-file exe unpacks itself on every start (4-18 s).
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());
        std::fs::write(mgr.ytdl_legacy(), b"one-file").unwrap();
        assert_eq!(mgr.ytdl_live(), mgr.ytdl_legacy(), "used until a folder build exists");
        assert_eq!(mgr.ytdl_live_sha256(), Some(sha256_hex(b"one-file")));

        stage_folder(&mgr, b"folder", "zipsha");
        mgr.apply_staged_ytdl().unwrap();
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"folder");
        assert_eq!(mgr.find_binary("yt-dlp"), Some(mgr.ytdl_live()));
        assert_eq!(ytdl_exe_in(dir.path()), mgr.ytdl_live(), "the start-up probe sees the same build");

        // The self-check's comparison (or an operator) can go back to it.
        mgr.rollback_ytdl().unwrap();
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"one-file");
        mgr.rollback_ytdl().unwrap();
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"folder");
    }

    #[test]
    fn a_zip_entry_outside_the_folder_is_refused() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("yt-dlp.exe", opts).unwrap();
            w.write_all(b"exe").unwrap();
            w.start_file("_internal/base.dll", opts).unwrap();
            w.write_all(b"dll").unwrap();
            w.finish().unwrap();
        }
        let dir = TempDir::new().unwrap();
        extract_zip_into(buf.get_ref(), &dir.path().join("out")).unwrap();
        assert_eq!(std::fs::read(dir.path().join("out").join("_internal").join("base.dll")).unwrap(), b"dll");

        let mut evil = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut evil);
            w.start_file("../escaped.exe", zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(b"x").unwrap();
            w.finish().unwrap();
        }
        assert!(extract_zip_into(evil.get_ref(), &dir.path().join("out2")).is_err());
        assert!(!dir.path().join("escaped.exe").exists());
    }

    #[test]
    fn a_first_install_with_no_live_binary_still_works() {
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());
        stage_folder(&mgr, b"first build", "aaa");

        mgr.apply_staged_ytdl().unwrap();
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"first build");
        assert!(!mgr.ytdl_dir_previous().exists());
    }
}

#[cfg(test)]
mod pinned_tests {
    use super::*;
    use crate::toolpins::{PinMember, ToolPin};
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct FakeFetcher {
        body: Vec<u8>,
        calls: AtomicUsize,
    }

    impl FakeFetcher {
        fn new(body: Vec<u8>) -> Self {
            Self { body, calls: AtomicUsize::new(0) }
        }
    }

    impl Fetcher for FakeFetcher {
        fn fetch<'a>(&'a self, _url: &'a str, sink: &'a mut (dyn FnMut(&[u8]) -> Result<()> + Send)) -> FetchFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                for chunk in self.body.chunks(7) {
                    sink(chunk)?;
                }
                Ok(())
            })
        }
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            for (name, data) in entries {
                z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(data).unwrap();
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// A pin for `archive` whose members are `(exe, entry_suffix, content)`.
    fn pin_for_test(archive: &[u8], members: &[(&'static str, &'static str, &[u8])], vcrt: bool) -> ToolPin {
        let members: Vec<PinMember> = members
            .iter()
            .map(|(exe, suffix, content)| PinMember { exe, entry_suffix: suffix, sha256: leak(sha256_hex(content)) })
            .collect();
        ToolPin {
            tool: "faketool",
            version: "1.0.0",
            url: "https://github.com/example/faketool.zip",
            size: archive.len() as u64,
            archive_sha256: None,
            members: Box::leak(members.into_boxed_slice()),
            probe_exe: "fake.exe",
            probe_args: &[],
            probe_contains: "",
            probe_first_line: false,
            needs_vcrt: vcrt,
        }
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    const DRAIN: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn a_member_that_is_not_the_validated_binary_is_refused_and_nothing_changes() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        std::fs::write(bin.join("fake.exe"), b"old build").unwrap();

        let archive = zip_of(&[("pkg/bin/fake.exe", b"a different build")]);
        let pin = pin_for_test(&archive, &[("fake.exe", "bin/fake.exe", b"the validated build")], false);
        let fetcher = FakeFetcher::new(archive);
        let gate = UpdateGate::new();

        let err = mgr.install_pinned(&pin, &fetcher, &gate, DRAIN, &ToolInstaller::new()).await.unwrap_err();
        assert!(format!("{err:#}").contains("not the binary that was validated"), "{err:#}");
        assert_eq!(std::fs::read(bin.join("fake.exe")).unwrap(), b"old build");
        assert_eq!(files_in(&bin), vec!["fake.exe"], "no .prev, no staged folder");
        assert!(!gate.is_paused());
    }

    #[tokio::test]
    async fn an_archive_with_the_wrong_hash_is_refused_before_anything_is_extracted() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        let archive = zip_of(&[("pkg/bin/fake.exe", b"build")]);
        let mut pin = pin_for_test(&archive, &[("fake.exe", "bin/fake.exe", b"build")], false);
        // The member is right; only the archive's pinned hash is wrong.
        pin.archive_sha256 = Some(leak("0".repeat(64)));
        let fetcher = FakeFetcher::new(archive);

        let err = mgr.install_pinned(&pin, &fetcher, &UpdateGate::new(), DRAIN, &ToolInstaller::new()).await.unwrap_err();
        assert!(format!("{err:#}").contains("archive failed its checksum"), "{err:#}");
        assert!(files_in(&bin).is_empty(), "{:?}", files_in(&bin));
    }

    #[tokio::test]
    async fn a_download_larger_or_smaller_than_the_pinned_size_is_refused() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        std::fs::write(bin.join("fake.exe"), b"old build").unwrap();
        let archive = zip_of(&[("pkg/bin/fake.exe", b"build")]);
        let pin = pin_for_test(&archive, &[("fake.exe", "bin/fake.exe", b"build")], false);

        let mut bigger = archive.clone();
        bigger.extend_from_slice(b"trailing bytes");
        let err = mgr
            .install_pinned(&pin, &FakeFetcher::new(bigger), &UpdateGate::new(), DRAIN, &ToolInstaller::new())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("larger than the pinned"), "{err:#}");

        let smaller = archive[..archive.len() - 3].to_vec();
        let err = mgr
            .install_pinned(&pin, &FakeFetcher::new(smaller), &UpdateGate::new(), DRAIN, &ToolInstaller::new())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("bytes, the pinned build is"), "{err:#}");

        assert_eq!(std::fs::read(bin.join("fake.exe")).unwrap(), b"old build");
        assert_eq!(files_in(&bin), vec!["fake.exe"]);
    }

    #[tokio::test]
    async fn the_pinned_build_replaces_the_live_one_and_the_old_one_is_kept() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        std::fs::write(bin.join("fake.exe"), b"old build").unwrap();
        std::fs::write(bin.join("fake.exe.prev"), b"older build").unwrap();

        let archive = zip_of(&[("pkg/bin/fake.exe", b"new build"), ("pkg/bin/fakeprobe.exe", b"new probe")]);
        let pin = pin_for_test(
            &archive,
            &[("fake.exe", "bin/fake.exe", b"new build"), ("fakeprobe.exe", "bin/fakeprobe.exe", b"new probe")],
            false,
        );
        let fetcher = FakeFetcher::new(archive);
        let gate = UpdateGate::new();
        let st = ToolInstaller::new();
        let guard = st.try_begin("faketool").unwrap();

        let out = mgr.install_pinned(&pin, &fetcher, &gate, DRAIN, &st).await.unwrap();
        assert!(!out.already);
        assert_eq!(std::fs::read(bin.join("fake.exe")).unwrap(), b"new build");
        assert_eq!(std::fs::read(bin.join("fakeprobe.exe")).unwrap(), b"new probe");
        assert_eq!(std::fs::read(bin.join("fake.exe.prev")).unwrap(), b"old build", "the older .prev is replaced");
        assert_eq!(files_in(&bin), vec!["fake.exe", "fake.exe.prev", "fakeprobe.exe"], "staged folder is gone");
        assert!(!gate.is_paused(), "the gate is resumed");
        let running = st.running().unwrap();
        assert_eq!((running.bytes_done, running.bytes_total), (pin.size, pin.size));
        guard.finish(InstallResult { ok: true, message: out.message, needs_restart: false });
        assert!(st.running().is_none());
    }

    #[tokio::test]
    async fn a_tool_already_at_the_pin_is_not_downloaded_again() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        std::fs::write(bin.join("fake.exe"), b"pinned build").unwrap();
        let archive = zip_of(&[("pkg/bin/fake.exe", b"pinned build")]);
        let pin = pin_for_test(&archive, &[("fake.exe", "bin/fake.exe", b"pinned build")], false);
        let fetcher = FakeFetcher::new(archive);

        let out = mgr.install_pinned(&pin, &fetcher, &UpdateGate::new(), DRAIN, &ToolInstaller::new()).await.unwrap();
        assert!(out.already);
        assert!(out.message.contains("already at the pinned version"));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
        assert_eq!(files_in(&bin), vec!["fake.exe"]);
    }

    #[tokio::test]
    async fn running_jobs_stop_the_swap_and_the_gate_is_reopened() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        std::fs::write(bin.join("fake.exe"), b"old build").unwrap();
        let archive = zip_of(&[("pkg/bin/fake.exe", b"new build")]);
        let pin = pin_for_test(&archive, &[("fake.exe", "bin/fake.exe", b"new build")], false);
        let fetcher = FakeFetcher::new(archive);
        let gate = UpdateGate::new();
        let _job = gate.try_enter().expect("gate open");

        let err = mgr
            .install_pinned(&pin, &fetcher, &gate, Duration::from_millis(100), &ToolInstaller::new())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("jobs still running; try again"), "{err:#}");
        assert_eq!(std::fs::read(bin.join("fake.exe")).unwrap(), b"old build");
        assert_eq!(files_in(&bin), vec!["fake.exe"], "staged discarded, no .prev");
        assert!(!gate.is_paused(), "the gate must be resumed, not left paused");
    }

    #[tokio::test]
    async fn only_the_pinned_members_are_ever_written_to_bin() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        let archive = zip_of(&[
            ("pkg/bin/evil.dll", b"not pinned"),
            ("pkg/bin/other.exe", b"not pinned either"),
            ("pkg/bin/fake.exe", b"build"),
        ]);
        let pin = pin_for_test(&archive, &[("fake.exe", "bin/fake.exe", b"build")], false);
        mgr.install_pinned(&pin, &FakeFetcher::new(archive), &UpdateGate::new(), DRAIN, &ToolInstaller::new())
            .await
            .unwrap();
        assert_eq!(files_in(&bin), vec!["fake.exe"]);
    }

    #[tokio::test]
    async fn an_entry_path_is_never_used_only_the_pinned_file_name() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        let archive = zip_of(&[("../../bmxtranswrap.exe", b"build"), ("..\\x.exe", b"nope")]);
        let pin = pin_for_test(&archive, &[("bmxtranswrap.exe", "bmxtranswrap.exe", b"build")], false);
        mgr.install_pinned(&pin, &FakeFetcher::new(archive), &UpdateGate::new(), DRAIN, &ToolInstaller::new())
            .await
            .unwrap();
        assert_eq!(files_in(&bin), vec!["bmxtranswrap.exe"]);
        assert_eq!(files_in(dir.path()), vec!["bin"], "nothing escaped the bin folder");
    }

    #[tokio::test]
    async fn a_missing_vc_runtime_is_reported_before_any_download() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let sys = dir.path().join("system32");
        std::fs::create_dir_all(&sys).unwrap();
        let mgr = DependencyManager::new(&bin).with_system_dir(&sys);
        let archive = zip_of(&[("bmxtranswrap.exe", b"build")]);
        let pin = pin_for_test(&archive, &[("bmxtranswrap.exe", "bmxtranswrap.exe", b"build")], true);
        let fetcher = FakeFetcher::new(archive);

        let err = mgr.install_pinned(&pin, &fetcher, &UpdateGate::new(), DRAIN, &ToolInstaller::new()).await.unwrap_err();
        assert!(format!("{err:#}").contains("VC++ 2015-2022 x64 redistributable"), "{err:#}");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
        assert!(files_in(&bin).is_empty());
    }

    #[tokio::test]
    async fn runtime_dlls_found_only_in_system32_are_copied_next_to_the_tool() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let sys = dir.path().join("system32");
        std::fs::create_dir_all(&sys).unwrap();
        for d in crate::toolpins::VCRT_DLLS {
            std::fs::write(sys.join(d), b"runtime").unwrap();
        }
        let mgr = DependencyManager::new(&bin).with_system_dir(&sys);
        // One DLL already in bin/ must not be overwritten.
        std::fs::write(bin.join("msvcp140.dll"), b"mine").unwrap();
        let archive = zip_of(&[("bmxtranswrap.exe", b"build")]);
        let pin = pin_for_test(&archive, &[("bmxtranswrap.exe", "bmxtranswrap.exe", b"build")], true);

        mgr.install_pinned(&pin, &FakeFetcher::new(archive), &UpdateGate::new(), DRAIN, &ToolInstaller::new())
            .await
            .unwrap();
        assert_eq!(std::fs::read(bin.join("msvcp140.dll")).unwrap(), b"mine");
        assert_eq!(std::fs::read(bin.join("vcruntime140.dll")).unwrap(), b"runtime");
        assert_eq!(std::fs::read(bin.join("vcruntime140_1.dll")).unwrap(), b"runtime");
        assert!(bin.join("bmxtranswrap.exe").is_file());
    }

    #[test]
    fn only_one_install_runs_at_a_time_and_a_dropped_one_frees_the_slot() {
        let st = ToolInstaller::new();
        let first = st.try_begin("ffmpeg").unwrap();
        assert_eq!(st.try_begin("deno").err().as_deref(), Some("ffmpeg"));
        drop(first);
        assert!(st.running().is_none());
        assert!(!st.last_result("ffmpeg").unwrap().ok, "an interrupted install is not a success");
        assert!(st.try_begin("deno").is_ok());
    }

    #[test]
    fn versions_are_read_from_what_each_tool_prints() {
        assert_eq!(
            parse_tool_version("ffmpeg", "ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright").as_deref(),
            Some("9.0.2-full_build-www.gyan.dev")
        );
        assert_eq!(parse_tool_version("bmxtranswrap", "bmx v1.6.0, libMXF").as_deref(), Some("1.6.0"));
        assert_eq!(parse_tool_version("yt-dlp", "\n2026.10.01\n").as_deref(), Some("2026.10.01"));
        assert_eq!(parse_tool_version("ffmpeg", "nothing"), None);
    }

    /// Passes the staged copy, fails the installed one: a build that unpacks and
    /// then does not run from where it lives.
    fn fails_once_installed() -> ProbeHook {
        Arc::new(|exe: &Path| exe.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "faketool.staged"))
    }

    #[tokio::test]
    async fn a_build_that_fails_its_check_after_the_swap_is_rolled_back() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin).with_probe_hook(fails_once_installed());
        std::fs::write(bin.join("fake.exe"), b"old build").unwrap();
        std::fs::write(bin.join("fakeprobe.exe"), b"old probe").unwrap();
        let archive = zip_of(&[("pkg/bin/fake.exe", b"new build"), ("pkg/bin/fakeprobe.exe", b"new probe")]);
        let pin = pin_for_test(
            &archive,
            &[("fake.exe", "bin/fake.exe", b"new build"), ("fakeprobe.exe", "bin/fakeprobe.exe", b"new probe")],
            false,
        );
        let gate = UpdateGate::new();

        let err = mgr
            .install_pinned(&pin, &FakeFetcher::new(archive), &gate, DRAIN, &ToolInstaller::new())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("the previous files were put back"), "{err:#}");
        assert_eq!(std::fs::read(bin.join("fake.exe")).unwrap(), b"old build");
        assert_eq!(std::fs::read(bin.join("fakeprobe.exe")).unwrap(), b"old probe");
        assert_eq!(files_in(&bin), vec!["fake.exe", "fakeprobe.exe"], "no .prev left, no staged folder");
        assert!(!gate.is_paused());
    }

    #[tokio::test]
    async fn when_the_second_member_cannot_be_swapped_the_first_is_put_back_too() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        std::fs::write(bin.join("fake.exe"), b"old build").unwrap();
        std::fs::write(bin.join("fakeprobe.exe"), b"old probe").unwrap();
        // A folder where the second member's .prev must go: moving the old file aside fails.
        std::fs::create_dir_all(bin.join("fakeprobe.exe.prev")).unwrap();
        std::fs::write(bin.join("fakeprobe.exe.prev").join("keep"), b"x").unwrap();
        let archive = zip_of(&[("pkg/bin/fake.exe", b"new build"), ("pkg/bin/fakeprobe.exe", b"new probe")]);
        let pin = pin_for_test(
            &archive,
            &[("fake.exe", "bin/fake.exe", b"new build"), ("fakeprobe.exe", "bin/fakeprobe.exe", b"new probe")],
            false,
        );
        let gate = UpdateGate::new();

        let err = mgr
            .install_pinned(&pin, &FakeFetcher::new(archive), &gate, DRAIN, &ToolInstaller::new())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("the previous files were put back"), "{err:#}");
        assert_eq!(std::fs::read(bin.join("fake.exe")).unwrap(), b"old build", "the first member is back");
        assert_eq!(std::fs::read(bin.join("fakeprobe.exe")).unwrap(), b"old probe");
        assert!(!bin.join("fake.exe.prev").exists());
        assert!(!bin.join("faketool.staged").exists());
        assert!(!gate.is_paused());
    }

    #[test]
    fn a_failed_rollback_says_so_and_names_the_file_that_is_still_there() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        // The live path is now a folder with content: it cannot be removed, so .prev cannot go back.
        std::fs::create_dir_all(bin.join("fake.exe")).unwrap();
        std::fs::write(bin.join("fake.exe").join("x"), b"x").unwrap();
        std::fs::write(bin.join("fake.exe.prev"), b"old build").unwrap();
        let moved = vec![Swapped { live: bin.join("fake.exe"), prev: bin.join("fake.exe.prev"), had_live: true }];

        let failed = mgr.undo_swap(&moved).unwrap_err();
        assert_eq!(rollback_failed(&failed), "rollback failed; fake.exe.prev is still there");
        assert!(bin.join("fake.exe.prev").is_file());
    }

    #[test]
    fn a_rollback_that_leaves_the_live_file_gone_says_that_too() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        // The new file is removable but the old one cannot be put back (it is not there at all).
        std::fs::write(bin.join("fake.exe"), b"new build").unwrap();
        let moved = vec![Swapped { live: bin.join("fake.exe"), prev: bin.join("fake.exe.prev"), had_live: true }];

        let failed = mgr.undo_swap(&moved).unwrap_err();
        assert_eq!(rollback_failed(&failed), "rollback failed; fake.exe is now missing; fake.exe.prev is still there");
        assert!(!bin.join("fake.exe").exists());
    }

    #[test]
    fn a_rollback_replaces_the_live_file_in_one_step() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        std::fs::write(bin.join("fake.exe"), b"new build").unwrap();
        std::fs::write(bin.join("fake.exe.prev"), b"old build").unwrap();
        let moved = vec![Swapped { live: bin.join("fake.exe"), prev: bin.join("fake.exe.prev"), had_live: true }];

        mgr.undo_swap(&moved).unwrap();
        assert_eq!(std::fs::read(bin.join("fake.exe")).unwrap(), b"old build");
        assert!(!bin.join("fake.exe.prev").exists());
    }

    #[tokio::test]
    async fn a_pinned_file_with_the_runtime_missing_is_reported_not_called_up_to_date() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let sys = dir.path().join("system32");
        std::fs::create_dir_all(&sys).unwrap();
        let mgr = DependencyManager::new(&bin).with_system_dir(&sys);
        std::fs::write(bin.join("bmxtranswrap.exe"), b"pinned build").unwrap();
        let archive = zip_of(&[("bmxtranswrap.exe", b"pinned build")]);
        let pin = pin_for_test(&archive, &[("bmxtranswrap.exe", "bmxtranswrap.exe", b"pinned build")], true);
        let fetcher = FakeFetcher::new(archive);

        let err = mgr.install_pinned(&pin, &fetcher, &UpdateGate::new(), DRAIN, &ToolInstaller::new()).await.unwrap_err();
        assert!(format!("{err:#}").contains("redistributable"), "{err:#}");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0, "a download would not help");

        // Present in System32 only: Repair copies it next to the tool and says so.
        for d in crate::toolpins::VCRT_DLLS {
            std::fs::write(sys.join(d), b"runtime").unwrap();
        }
        let out = mgr.install_pinned(&pin, &fetcher, &UpdateGate::new(), DRAIN, &ToolInstaller::new()).await.unwrap();
        assert!(out.already && out.message.contains("copied from System32"), "{}", out.message);
        assert!(bin.join("vcruntime140.dll").is_file());
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_pinned_file_that_does_not_run_is_an_error_not_a_loop() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin).with_probe_hook(Arc::new(|_: &Path| false));
        std::fs::write(bin.join("fake.exe"), b"pinned build").unwrap();
        let archive = zip_of(&[("pkg/bin/fake.exe", b"pinned build")]);
        let pin = pin_for_test(&archive, &[("fake.exe", "bin/fake.exe", b"pinned build")], false);
        let fetcher = FakeFetcher::new(archive);

        let err = mgr.install_pinned(&pin, &fetcher, &UpdateGate::new(), DRAIN, &ToolInstaller::new()).await.unwrap_err();
        assert!(format!("{err:#}").contains("pinned hash"), "{err:#}");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_prev_without_a_staged_folder_is_never_put_back_and_the_tool_stays_missing_and_amber() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        // A finished swap, then the new file was quarantined or deleted.
        std::fs::write(bin.join("ffmpeg.exe.prev"), b"previous ffmpeg").unwrap();
        std::fs::write(bin.join("deno.exe.prev"), b"previous deno").unwrap();

        assert!(mgr.recover_interrupted_swaps().is_empty());
        assert!(!bin.join("ffmpeg.exe").exists(), "the previous build must not come back on air");
        assert!(bin.join("ffmpeg.exe.prev").is_file());
        assert_eq!(mgr.find_binary("ffmpeg"), None, "no .prev and no PATH lookup while a .prev exists");

        let check = mgr.pinned_tools_check(&ToolInstaller::new(), &HashMap::new());
        assert_eq!(check.state, crate::health::Health::Degraded);
        assert!(
            check.detail.as_deref().unwrap().contains("ffmpeg is not the pinned build: Admin \u{2192} Tools \u{2192} Repair"),
            "{:?}",
            check.detail
        );
    }

    /// Two members: `fake.exe` already finished, `fakeprobe.exe` not (the crash fell between them).
    fn mixed_crash(dir: &Path, staged_probe: &[u8]) -> (DependencyManager, ToolPin, PathBuf) {
        let bin = dir.join("bin");
        let mgr = DependencyManager::new(&bin);
        let archive = zip_of(&[("x", b"x")]);
        let pin = pin_for_test(
            &archive,
            &[("fake.exe", "bin/fake.exe", b"new build"), ("fakeprobe.exe", "bin/fakeprobe.exe", b"new probe")],
            false,
        );
        std::fs::write(bin.join("fake.exe"), b"new build").unwrap();
        std::fs::write(bin.join("fakeprobe.exe.prev"), b"old probe").unwrap();
        let staged = bin.join("faketool.staged");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("fakeprobe.exe"), staged_probe).unwrap();
        (mgr, pin, bin)
    }

    #[test]
    fn a_staged_member_that_matches_the_pin_finishes_the_interrupted_swap_forward() {
        let dir = TempDir::new().unwrap();
        let (mgr, pin, bin) = mixed_crash(dir.path(), b"new probe");

        let done = mgr.recover_interrupted_swaps_for(std::slice::from_ref(&pin));
        assert_eq!(done, vec!["fakeprobe.exe".to_string()]);
        assert_eq!(std::fs::read(bin.join("fake.exe")).unwrap(), b"new build");
        assert_eq!(std::fs::read(bin.join("fakeprobe.exe")).unwrap(), b"new probe", "both end up pinned");
        assert!(!bin.join("faketool.staged").exists());
        let check = mgr.pinned_tools_check_for(std::slice::from_ref(&pin), &ToolInstaller::new(), &HashMap::new());
        assert_eq!(check.state, crate::health::Health::Ok, "{:?}", check.detail);
    }

    #[test]
    fn a_staged_member_with_the_wrong_hash_is_discarded_and_the_tool_stays_missing() {
        let dir = TempDir::new().unwrap();
        let (mgr, pin, bin) = mixed_crash(dir.path(), b"half written");

        assert!(mgr.recover_interrupted_swaps_for(std::slice::from_ref(&pin)).is_empty());
        assert!(!bin.join("fakeprobe.exe").exists(), "neither the bad staged file nor the .prev goes live");
        assert!(!bin.join("faketool.staged").exists(), "the staged folder is discarded");
        assert!(bin.join("fakeprobe.exe.prev").is_file());
        let check = mgr.pinned_tools_check_for(std::slice::from_ref(&pin), &ToolInstaller::new(), &HashMap::new());
        assert_eq!(check.state, crate::health::Health::Degraded);
        assert!(check.detail.as_deref().unwrap().contains("fakeprobe is not the pinned build"), "{:?}", check.detail);
    }

    #[test]
    fn a_staged_member_replaces_an_old_live_file_which_becomes_prev() {
        let dir = TempDir::new().unwrap();
        let (mgr, pin, bin) = mixed_crash(dir.path(), b"new probe");
        std::fs::write(bin.join("fakeprobe.exe"), b"old probe").unwrap();
        std::fs::write(bin.join("fakeprobe.exe.prev"), b"older probe").unwrap();

        assert_eq!(mgr.recover_interrupted_swaps_for(std::slice::from_ref(&pin)).len(), 1);
        assert_eq!(std::fs::read(bin.join("fakeprobe.exe")).unwrap(), b"new probe");
        assert_eq!(std::fs::read(bin.join("fakeprobe.exe.prev")).unwrap(), b"old probe");
    }

    #[test]
    fn a_live_file_that_is_not_the_pinned_build_makes_the_health_line_amber() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        std::fs::write(bin.join("ffmpeg.exe"), b"some other ffmpeg").unwrap();
        let elsewhere = dir.path().join("ffprobe.exe");
        std::fs::write(&elsewhere, b"some other ffprobe").unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("ffprobe".to_string(), elsewhere);

        let check = mgr.pinned_tools_check(&ToolInstaller::new(), &overrides);
        assert_eq!(check.state, crate::health::Health::Degraded);
        let detail = check.detail.unwrap();
        assert!(detail.contains("ffmpeg is not the pinned build: Admin \u{2192} Tools \u{2192} Repair"), "{detail}");
        assert!(detail.contains("ffprobe is not the pinned build (set in config.json: "), "{detail}");
        assert!(detail.contains("bmxtranswrap is not the pinned build"), "{detail}");

        // A pinned file is green.
        let archive = zip_of(&[("x", b"x")]);
        let pin = pin_for_test(&archive, &[("fake.exe", "bin/fake.exe", b"pinned")], false);
        std::fs::write(bin.join("fake.exe"), b"pinned").unwrap();
        let ok = mgr.pinned_tools_check_for(std::slice::from_ref(&pin), &ToolInstaller::new(), &HashMap::new());
        assert_eq!(ok.state, crate::health::Health::Ok);
    }

    #[tokio::test]
    async fn a_config_override_is_what_the_status_reports_and_repair_concerns_only_bin() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        let mgr = DependencyManager::new(&bin);
        let elsewhere = dir.path().join("other").join("ffmpeg.exe");
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, b"a different ffmpeg").unwrap();
        std::fs::write(bin.join("ffmpeg.exe"), b"stale copy in bin").unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("ffmpeg".to_string(), elsewhere.clone());

        let rows = mgr.tool_statuses(&ToolInstaller::new(), &overrides).await;
        let ffmpeg = rows.iter().find(|r| r.name == "ffmpeg").unwrap();
        assert_eq!(ffmpeg.path, elsewhere.to_string_lossy());
        assert_eq!(ffmpeg.source, "config");
        assert!(ffmpeg.note.as_deref().unwrap().starts_with("set in config.json: "), "{:?}", ffmpeg.note);
        assert_eq!(ffmpeg.matches_pin, Some(false), "computed on the override file");
        assert_eq!(ffmpeg.action, Some("repair"), "the stale copy in bin/ is what Repair can fix");

        let ffprobe = rows.iter().find(|r| r.name == "ffprobe").unwrap();
        assert_eq!((ffprobe.source, ffprobe.note.is_none()), ("bin", true));
    }
}
