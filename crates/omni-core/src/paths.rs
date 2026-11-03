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

    /// Where a finished video waits for MCR's approval (P10.5): one
    /// directory per job, removed as a whole, never matched by prefix.
    pub fn held(&self, job_id: i64) -> PathBuf {
        self.temp.join("held").join(job_id.to_string())
    }

    /// Scratch space of one benchmark target (P6.4): `temp/benchmark/{run}/{idx}`,
    /// removed as a whole when the target is done. Never a queue job's directory.
    pub fn benchmark_target_temp(&self, run_id: i64, idx: i64) -> PathBuf {
        self.benchmark_temp().join(run_id.to_string()).join(idx.to_string())
    }

    /// All benchmark scratch space; only one run exists at a time.
    pub fn benchmark_temp(&self) -> PathBuf {
        self.temp.join("benchmark")
    }

    /// Where a benchmark run's `report.json` and `report.md` are written.
    pub fn benchmark_report_dir(&self, run_id: i64) -> PathBuf {
        self.data.join("benchmarks").join(run_id.to_string())
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

/// What Windows says about a drive letter's root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    /// `DRIVE_REMOTE`: a mapped network drive.
    Remote,
    /// `DRIVE_NO_ROOT_DIR`: no such drive for this process.
    NoRootDir,
    /// Anything else (fixed, removable, unknown).
    Other,
}

/// The two Windows lookups `watchfolder_problem` needs, behind a trait so tests
/// can inject them.
pub trait DriveLookup {
    /// `GetDriveTypeW` for `X:\`.
    fn drive_kind(&self, letter: char) -> DriveKind;
    /// `WNetGetConnectionW` for `X:`: the `\\server\share` it maps to.
    fn connection(&self, letter: char) -> Option<String>;
    /// `HKCU\Network\X\RemotePath`: the persistent mapping of the user who
    /// ran this process. An elevated token cannot see that user's drives
    /// (`GetDriveTypeW` says NO_ROOT_DIR) but loads the same HKCU.
    fn persistent_mapping(&self, letter: char) -> Option<String>;
}

/// The real lookups. Off Windows there are no drive letters to ask about.
pub struct SystemDrives;

#[cfg(windows)]
impl DriveLookup for SystemDrives {
    fn drive_kind(&self, letter: char) -> DriveKind {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetDriveTypeW;
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain(std::iter::once(0)).collect();
        // DRIVE_NO_ROOT_DIR = 1, DRIVE_REMOTE = 4.
        match unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } {
            4 => DriveKind::Remote,
            1 => DriveKind::NoRootDir,
            _ => DriveKind::Other,
        }
    }

    fn connection(&self, letter: char) -> Option<String> {
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
        let local: Vec<u16> = format!("{letter}:").encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let mut rc = unsafe { WNetGetConnectionW(PCWSTR(local.as_ptr()), PWSTR(buf.as_mut_ptr()), &mut len) };
        if rc.0 == 234 {
            // ERROR_MORE_DATA: `len` now holds the size needed.
            buf = vec![0u16; (len as usize).max(1024) + 1];
            len = buf.len() as u32;
            rc = unsafe { WNetGetConnectionW(PCWSTR(local.as_ptr()), PWSTR(buf.as_mut_ptr()), &mut len) };
        }
        // 1201 (ERROR_CONNECTION_UNAVAIL): remembered but disconnected; the
        // buffer is still filled.
        if rc.0 != 0 && rc.0 != 1201 {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..end]);
        if s.is_empty() { None } else { Some(s) }
    }

    fn persistent_mapping(&self, letter: char) -> Option<String> {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
        let key: Vec<u16> = format!("Network\\{letter}").encode_utf16().chain(std::iter::once(0)).collect();
        let name: Vec<u16> = "RemotePath".encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                Some(&mut bytes),
            )
        };
        if rc.0 != 0 {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..end]);
        if s.is_empty() { None } else { Some(s) }
    }
}

#[cfg(not(windows))]
impl DriveLookup for SystemDrives {
    fn drive_kind(&self, _letter: char) -> DriveKind {
        DriveKind::Other
    }
    fn connection(&self, _letter: char) -> Option<String> {
        None
    }
    fn persistent_mapping(&self, _letter: char) -> Option<String> {
        None
    }
}

/// The drive letter of a `X:` or `X:\...` path.
fn drive_letter(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    (c.is_ascii_alphabetic() && chars.next() == Some(':')).then(|| c.to_ascii_uppercase())
}

/// True when the path is on a mapped network drive (`DRIVE_REMOTE`).
pub fn is_mapped_drive(path: &str) -> bool {
    is_mapped_drive_with(path, &SystemDrives)
}

pub fn is_mapped_drive_with(path: &str, drives: &dyn DriveLookup) -> bool {
    let p = strip_device_prefix(path.trim());
    drive_letter(p).is_some_and(|l| match drives.drive_kind(l) {
        DriveKind::Remote => true,
        // An elevated process does not see the desktop's mapping; the
        // registry still remembers it.
        DriveKind::NoRootDir => drives.persistent_mapping(l).is_some(),
        DriveKind::Other => false,
    })
}

/// `\\?\Z:\x` and `\\.\Z:\x` name the same drive as `Z:\x`; `\\?\UNC\...` is left alone.
fn strip_device_prefix(p: &str) -> &str {
    for prefix in [r"\\?\", r"\\.\", "//?/", "//./"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            if drive_letter(rest).is_some() {
                return rest;
            }
        }
    }
    p
}

/// Why `path` cannot be the watchfolder, in plain English; `None` when it is
/// fine. A mapped drive letter belongs to one user's logon session: the
/// service runs in its own session and cannot see it.
pub fn watchfolder_problem(path: &str) -> Option<String> {
    watchfolder_problem_with(path, &SystemDrives)
}

pub fn watchfolder_problem_with(path: &str, drives: &dyn DriveLookup) -> Option<String> {
    const HINT: &str = r"a full path such as \\server\share\folder or D:\folder";
    let p = strip_device_prefix(path.trim());
    if p.is_empty() {
        return Some(format!("No watchfolder was given. Enter {HINT}."));
    }
    if is_unc(p) {
        return None;
    }
    let relative = || Some(format!("\"{p}\" is a relative path. Enter {HINT}."));
    let Some(letter) = drive_letter(p) else {
        return relative();
    };
    // `Z:folder` (no separator) is relative to that drive's current directory.
    let rest = &p[2..];
    if !rest.is_empty() && !rest.starts_with(['\\', '/']) {
        return relative();
    }
    // The share a drive letter points at, as a UNC path with the rest of the path.
    let suggestion = |share: String| {
        let rest = rest.trim_start_matches(['\\', '/']).replace('/', r"\");
        let share = share.trim_end_matches('\\').to_string();
        let full = if rest.is_empty() { share.clone() } else { format!(r"{share}\{rest}") };
        (share, full)
    };
    match drives.drive_kind(letter) {
        DriveKind::Remote => Some(match drives.connection(letter) {
            Some(share) => {
                let (share, full) = suggestion(share);
                format!(
                    "{letter}: is a mapped network drive ({share}). The service cannot see drive mappings; use {full} instead."
                )
            }
            None => format!(
                r"{letter}: is a mapped network drive. The service cannot see drive mappings; use the \\server\share path of that drive instead."
            ),
        }),
        DriveKind::NoRootDir => Some(match drives.persistent_mapping(letter) {
            Some(share) => {
                let (share, full) = suggestion(share);
                format!(
                    "{letter}: is a mapped network drive on your desktop ({share}); the service cannot see drive mappings; use {full} instead."
                )
            }
            None => format!(
                r"Drive {letter}: does not exist for this process. If it is a mapped drive on your desktop, use its \\server\share path instead."
            ),
        }),
        // A bare `D:` is the drive's current directory, not its root.
        DriveKind::Other if rest.is_empty() => relative(),
        DriveKind::Other => None,
    }
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

    struct Fake {
        kind: DriveKind,
        share: Option<&'static str>,
        persistent: Option<&'static str>,
    }
    impl DriveLookup for Fake {
        fn drive_kind(&self, letter: char) -> DriveKind {
            if letter == 'D' { DriveKind::Other } else { self.kind }
        }
        fn connection(&self, _letter: char) -> Option<String> {
            self.share.map(str::to_string)
        }
        fn persistent_mapping(&self, _letter: char) -> Option<String> {
            self.persistent.map(str::to_string)
        }
    }
    const MAPPED: Fake =
        Fake { kind: DriveKind::Remote, share: Some(r"\\192.168.4.42\FileIngest"), persistent: None };

    #[test]
    fn an_elevated_process_still_gets_the_path_from_the_registry_mapping() {
        // Elevated tokens cannot see the desktop's Z: (NO_ROOT_DIR) but read the same HKCU.
        let elevated = Fake {
            kind: DriveKind::NoRootDir,
            share: None,
            persistent: Some(r"\\192.168.4.42\FileIngest"),
        };
        assert_eq!(
            watchfolder_problem_with(r"Z:\Dalet", &elevated).unwrap(),
            r"Z: is a mapped network drive on your desktop (\\192.168.4.42\FileIngest); the service cannot see drive mappings; use \\192.168.4.42\FileIngest\Dalet instead."
        );
        assert!(is_mapped_drive_with(r"Z:\Dalet", &elevated));
    }

    #[test]
    fn a_bare_drive_is_relative_and_a_device_prefix_does_not_hide_a_mapping() {
        assert!(watchfolder_problem_with("D:", &MAPPED).unwrap().contains("relative path"));
        for p in [r"\\?\Z:\x", r"\\.\Z:\x", "//?/Z:/x"] {
            let m = watchfolder_problem_with(p, &MAPPED).unwrap_or_default();
            assert!(m.contains(r"use \\192.168.4.42\FileIngest\x instead"), "{p}: {m}");
            assert!(is_mapped_drive_with(p, &MAPPED), "{p}");
        }
        assert_eq!(watchfolder_problem_with(r"\\?\D:\x", &MAPPED), None);
        assert_eq!(watchfolder_problem_with(r"\\?\UNC\nas\ingest", &MAPPED), None);
    }

    #[test]
    fn a_mapped_drive_names_the_unc_path_to_use_including_the_sub_path() {
        let m = watchfolder_problem_with(r"Z:\Dalet\Ingest", &MAPPED).unwrap();
        assert_eq!(
            m,
            r"Z: is a mapped network drive (\\192.168.4.42\FileIngest). The service cannot see drive mappings; use \\192.168.4.42\FileIngest\Dalet\Ingest instead."
        );
        let root = watchfolder_problem_with("z:", &MAPPED).unwrap();
        assert!(root.contains(r"use \\192.168.4.42\FileIngest instead."), "{root}");
        assert!(is_mapped_drive_with(r"Z:\x", &MAPPED));
        assert!(!is_mapped_drive_with(r"D:\x", &MAPPED));
    }

    #[test]
    fn a_missing_drive_is_explained() {
        let none = Fake { kind: DriveKind::NoRootDir, share: None, persistent: None };
        assert_eq!(
            watchfolder_problem_with(r"Y:\x", &none).unwrap(),
            r"Drive Y: does not exist for this process. If it is a mapped drive on your desktop, use its \\server\share path instead."
        );
    }

    #[test]
    fn empty_and_relative_paths_are_refused() {
        for p in ["", "   ", "watchfolder", r"..\x", "Z:x"] {
            let m = watchfolder_problem_with(p, &MAPPED).unwrap_or_default();
            assert!(m.contains(r"\\server\share\folder"), "{p:?}: {m}");
        }
    }

    #[test]
    fn local_and_unc_paths_are_fine() {
        assert_eq!(watchfolder_problem_with(r"D:\x", &MAPPED), None);
        assert_eq!(watchfolder_problem_with(r"\\dalet\ingest", &MAPPED), None);
        assert_eq!(watchfolder_problem_with("//dalet/ingest", &MAPPED), None);
    }
}
