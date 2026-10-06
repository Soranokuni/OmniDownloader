use anyhow::{Context, Result};
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
            // yt-dlp's JavaScript runtime for YouTube (plan P6.8).
            ("deno", "deno.exe", "--version"),
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

    /// Path of the live binary, the staged replacement, and the previous one.
    pub fn ytdl_live(&self) -> PathBuf {
        self.bin_dir.join("yt-dlp.exe")
    }
    pub fn ytdl_staged(&self) -> PathBuf {
        self.bin_dir.join("yt-dlp.exe.staged")
    }
    pub fn ytdl_previous(&self) -> PathBuf {
        self.bin_dir.join("yt-dlp.exe.prev")
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
    /// Returns the staged path and the verified hash.
    pub async fn stage_ytdl(&self, channel: &str) -> Result<StagedTool> {
        let base = if channel == "nightly" {
            "https://github.com/yt-dlp/yt-dlp-nightly-builds/releases/latest/download"
        } else {
            "https://github.com/yt-dlp/yt-dlp/releases/latest/download"
        };
        let exe_url = format!("{base}/yt-dlp.exe");
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

        let expected = expected_sha256(&sums, "yt-dlp.exe").ok_or_else(|| {
            anyhow::anyhow!(
                "SHA2-256SUMS from the yt-dlp release has no entry for yt-dlp.exe; refusing to \
                 install an unverified binary"
            )
        })?;

        info!("Downloading yt-dlp from {}", exe_url);
        let bytes = client
            .get(&exe_url)
            .send()
            .await
            .context("Failed downloading yt-dlp.exe")?
            .error_for_status()
            .context("yt-dlp.exe request failed")?
            .bytes()
            .await?;

        let actual = sha256_hex(&bytes);
        if actual != expected {
            anyhow::bail!(
                "yt-dlp.exe failed checksum verification (expected {expected}, got {actual}, \
                 {} bytes). Nothing was installed.",
                bytes.len()
            );
        }

        let staged = self.ytdl_staged();
        tokio::fs::write(&staged, &bytes)
            .await
            .with_context(|| format!("Failed writing {staged:?}"))?;

        info!(
            "Staged a verified yt-dlp ({} bytes, sha256 {}) at {:?}",
            bytes.len(),
            &actual[..16],
            staged
        );
        Ok(StagedTool {
            path: staged,
            sha256: actual,
            bytes: bytes.len() as u64,
        })
    }

    /// Swap a staged yt-dlp into place, keeping the old one as `.prev`.
    ///
    /// The caller is responsible for there being no download in flight; see
    /// the update gate in `main.rs`. Rename-based, so the window in which
    /// neither file is at the live path is as short as the filesystem allows.
    pub fn apply_staged_ytdl(&self) -> Result<PathBuf> {
        let staged = self.ytdl_staged();
        if !staged.exists() {
            anyhow::bail!("No staged yt-dlp to apply at {staged:?}");
        }
        let live = self.ytdl_live();
        let prev = self.ytdl_previous();

        if live.exists() {
            let _ = std::fs::remove_file(&prev);
            std::fs::rename(&live, &prev)
                .with_context(|| format!("Failed moving {live:?} aside to {prev:?}"))?;
        }
        if let Err(e) = std::fs::rename(&staged, &live) {
            // Put the old binary back rather than leaving the station with no
            // downloader at all.
            if prev.exists() {
                let _ = std::fs::rename(&prev, &live);
            }
            return Err(e).with_context(|| format!("Failed installing {staged:?} as {live:?}"));
        }
        info!("yt-dlp updated; previous build kept at {:?}", prev);
        Ok(live)
    }

    /// Put `.prev` back. The admin panel's "Rollback" after a bad release.
    pub fn rollback_ytdl(&self) -> Result<PathBuf> {
        let prev = self.ytdl_previous();
        if !prev.exists() {
            anyhow::bail!("No previous yt-dlp to roll back to at {prev:?}");
        }
        let live = self.ytdl_live();
        // The build being rolled back becomes the new `.prev`, so a rollback
        // can be undone.
        let staged = self.ytdl_staged();
        if live.exists() {
            let _ = std::fs::remove_file(&staged);
            std::fs::rename(&live, &staged)?;
        }
        std::fs::rename(&prev, &live)?;
        if staged.exists() {
            let _ = std::fs::rename(&staged, &prev);
        }
        info!("Rolled yt-dlp back to the previous build");
        Ok(live)
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

    /// Download the latest Deno, check it against the release's SHA-256,
    /// prove it runs, and put it in place (the old one kept as `.prev`).
    /// Returns the installed version.
    pub async fn install_deno(&self) -> Result<String> {
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
        let staged = self.bin_dir.join("deno.exe.staged");
        std::fs::write(&staged, &exe).with_context(|| format!("Failed writing {staged:?}"))?;
        let version = parse_deno_version(&self.get_tool_version(&staged, "deno", "--version"));
        let Some(version) = version else {
            let _ = std::fs::remove_file(&staged);
            anyhow::bail!("the downloaded Deno does not run on this machine; nothing was installed");
        };

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
        info!("Deno {version} installed at {live:?} (sha256 {})", &actual[..16]);
        Ok(version)
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

    #[test]
    fn applying_a_staged_build_keeps_the_previous_one_for_rollback() {
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());

        std::fs::write(mgr.ytdl_live(), b"old build").unwrap();
        std::fs::write(mgr.ytdl_staged(), b"new build").unwrap();

        mgr.apply_staged_ytdl().unwrap();
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"new build");
        assert_eq!(std::fs::read(mgr.ytdl_previous()).unwrap(), b"old build");
        assert!(!mgr.ytdl_staged().exists());

        // A yt-dlp release that breaks an extractor is a Tuesday. Rolling back
        // must not require finding the old exe by hand.
        mgr.rollback_ytdl().unwrap();
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"old build");
        // ...and the rollback itself is reversible.
        assert_eq!(std::fs::read(mgr.ytdl_previous()).unwrap(), b"new build");
    }

    #[test]
    fn applying_with_nothing_staged_is_an_error_not_a_silent_no_op() {
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());
        std::fs::write(mgr.ytdl_live(), b"old build").unwrap();

        assert!(mgr.apply_staged_ytdl().is_err());
        // The working binary is untouched.
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"old build");
    }

    #[test]
    fn rollback_without_a_previous_build_refuses_rather_than_removing_the_live_one() {
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());
        std::fs::write(mgr.ytdl_live(), b"only build").unwrap();

        assert!(mgr.rollback_ytdl().is_err());
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"only build");
    }

    #[test]
    fn a_first_install_with_no_live_binary_still_works() {
        let dir = TempDir::new().unwrap();
        let mgr = DependencyManager::new(dir.path());
        std::fs::write(mgr.ytdl_staged(), b"first build").unwrap();

        mgr.apply_staged_ytdl().unwrap();
        assert_eq!(std::fs::read(mgr.ytdl_live()).unwrap(), b"first build");
        assert!(!mgr.ytdl_previous().exists());
    }
}
