//! Install-directory anchored path resolution (plan P0.1, defect W-10).
//!
//! Every relative path in `config.json` is resolved against the directory that
//! holds `omni-ingest.exe`, never against the process working directory. Under
//! the Windows Service Control Manager the CWD is `C:\Windows\System32`, so a
//! CWD-relative `data/omni.db` would silently create a second, empty database
//! there — and the operator would see an empty queue with no error.
//!
//! Resolution order for the install root:
//!   1. explicit `--root <dir>` (passed to [`AppPaths::with_root`])
//!   2. the `OMNI_ROOT` environment variable
//!   3. the parent directory of `std::env::current_exe()`
//!   4. the current working directory (last resort, e.g. exotic test harnesses)
//!
//! `cargo test` and `cargo run` place the executable in `target/{debug,release}`,
//! which would put runtime state inside the build directory. When the executable
//! sits in a Cargo target directory we walk up to the workspace root instead, so
//! development behaves like an installed deployment.

use std::path::{Path, PathBuf};

/// Absolute locations of every directory the daemon writes to.
///
/// Construct once during start-up and pass down; nothing below should call
/// `std::env::current_dir()`.
#[derive(Debug, Clone)]
pub struct AppPaths {
    /// Install directory. All relative config paths resolve against this.
    pub root: PathBuf,
    /// `config.json` (or whatever `--config` named).
    pub config: PathBuf,
    /// SQLite database, adblock cache, secrets, benchmark reports.
    pub data: PathBuf,
    /// Per-job scratch space (`temp/jobs/{id}`).
    pub temp: PathBuf,
    /// ffmpeg, ffprobe, bmxtranswrap, yt-dlp.
    pub bin: PathBuf,
    /// Rolling log files.
    pub logs: PathBuf,
    /// Delivered source archive.
    pub archive: PathBuf,
}

impl AppPaths {
    /// Discover the install root and derive the standard subdirectories.
    ///
    /// `config_arg` is the `--config` value: absolute paths are honoured as
    /// given, relative ones resolve against the discovered root.
    pub fn discover(config_arg: &str) -> Self {
        Self::with_root(Self::discover_root(), config_arg)
    }

    /// Build the path set for an explicit root. Used by `--root` and by tests.
    pub fn with_root<P: AsRef<Path>>(root: P, config_arg: &str) -> Self {
        let root = normalize(root.as_ref());
        let config = resolve_against(&root, config_arg);
        Self {
            data: root.join("data"),
            temp: root.join("temp"),
            bin: root.join("bin"),
            logs: root.join("logs"),
            archive: root.join("archive"),
            config,
            root,
        }
    }

    /// Resolve one relative-or-absolute path against the install root.
    ///
    /// This is the single place that decides what a relative config path means.
    pub fn resolve(&self, relative_or_absolute: &str) -> PathBuf {
        resolve_against(&self.root, relative_or_absolute)
    }

    /// Scratch directory for a single job (plan P1.4, defect D-04).
    ///
    /// Cleanup is `remove_dir_all` of exactly this directory, never a filename
    /// prefix match — job 1's prefix `1_` also matches jobs 10-19 and 100-199.
    pub fn job_temp(&self, job_id: i64) -> PathBuf {
        self.temp.join("jobs").join(job_id.to_string())
    }

    /// Create every directory the daemon writes to.
    ///
    /// Called once at start-up so a missing `logs/` does not surface later as a
    /// failure in the middle of a job.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [
            &self.data,
            &self.temp,
            &self.logs,
            &self.archive,
            &self.temp.join("jobs"),
            &self.data.join("adblock"),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    fn discover_root() -> PathBuf {
        if let Some(env_root) = std::env::var_os("OMNI_ROOT") {
            let p = PathBuf::from(env_root);
            if !p.as_os_str().is_empty() {
                return p;
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return strip_cargo_target(dir);
            }
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// Walk up out of `target/{debug,release}[/deps]` to the workspace root.
///
/// Without this, `cargo run` would write `target/release/data/omni.db` and a
/// `cargo clean` would wipe the operator's queue.
fn strip_cargo_target(dir: &Path) -> PathBuf {
    let mut current = dir;
    // `target/debug/deps/foo-hash.exe` is two levels below the profile dir.
    for _ in 0..3 {
        let is_profile_dir = current
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == "debug" || n == "release" || n == "deps")
            .unwrap_or(false);
        if !is_profile_dir {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    if current.file_name().and_then(|n| n.to_str()) == Some("target") {
        if let Some(parent) = current.parent() {
            return parent.to_path_buf();
        }
    }
    dir.to_path_buf()
}

fn resolve_against(root: &Path, candidate: &str) -> PathBuf {
    let p = Path::new(candidate);
    if p.is_absolute() || is_unc(candidate) {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// `Path::is_absolute` already accepts `\\server\share` on Windows, but the
/// check is cheap and keeps UNC handling explicit for readers on other targets.
fn is_unc(s: &str) -> bool {
    s.starts_with("\\\\") || s.starts_with("//")
}

/// Make the root absolute without requiring it to exist (`canonicalize` fails on
/// a directory that has not been created yet, and on Windows it prefixes the
/// `\\?\` verbatim form, which some external tools mishandle).
fn normalize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_resolve_against_root_not_cwd() {
        let paths = AppPaths::with_root("C:/omni", "config.json");
        assert_eq!(paths.config, PathBuf::from("C:/omni").join("config.json"));
        assert_eq!(paths.resolve("data/omni.db"), PathBuf::from("C:/omni").join("data/omni.db"));
        assert_eq!(paths.data, PathBuf::from("C:/omni").join("data"));
        assert_eq!(paths.bin, PathBuf::from("C:/omni").join("bin"));
    }

    #[test]
    fn absolute_and_unc_paths_pass_through() {
        let paths = AppPaths::with_root("C:/omni", "config.json");
        // The Dalet watchfolder is a share; it must never be rebased on root.
        assert_eq!(
            paths.resolve("\\\\dalet\\ingest"),
            PathBuf::from("\\\\dalet\\ingest")
        );
        assert_eq!(paths.resolve("D:/media/out"), PathBuf::from("D:/media/out"));
    }

    #[test]
    fn explicit_absolute_config_wins_over_root() {
        let paths = AppPaths::with_root("C:/omni", "D:/staging/dev_config.json");
        assert_eq!(paths.config, PathBuf::from("D:/staging/dev_config.json"));
        // ...but data still lives under the install root.
        assert_eq!(paths.data, PathBuf::from("C:/omni").join("data"));
    }

    #[test]
    fn job_temp_is_per_job_directory() {
        let paths = AppPaths::with_root("C:/omni", "config.json");
        assert_eq!(paths.job_temp(7), PathBuf::from("C:/omni/temp/jobs/7"));
        // Job 1 and job 19 must not share a path component that a prefix match
        // would conflate (defect D-04).
        assert_ne!(paths.job_temp(1), paths.job_temp(19));
        assert!(!paths.job_temp(19).starts_with(paths.job_temp(1)));
    }

    #[test]
    fn cargo_target_dirs_resolve_to_workspace_root() {
        assert_eq!(
            strip_cargo_target(Path::new("D:/OmniDownloader/target/release")),
            PathBuf::from("D:/OmniDownloader")
        );
        assert_eq!(
            strip_cargo_target(Path::new("D:/OmniDownloader/target/debug/deps")),
            PathBuf::from("D:/OmniDownloader")
        );
        // A real install directory that merely ends in something else is kept.
        assert_eq!(
            strip_cargo_target(Path::new("C:/Program Files/OmniDownloader")),
            PathBuf::from("C:/Program Files/OmniDownloader")
        );
    }

    #[test]
    fn ensure_dirs_creates_the_full_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(tmp.path(), "config.json");
        paths.ensure_dirs().unwrap();
        for dir in [&paths.data, &paths.temp, &paths.logs, &paths.archive] {
            assert!(dir.is_dir(), "{dir:?} was not created");
        }
        assert!(paths.temp.join("jobs").is_dir());
    }
}
