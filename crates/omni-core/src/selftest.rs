//! Start-up self-test (plan P6.2).
//!
//! Every one of these failures used to surface as a *job* failure, minutes or
//! hours later, attributed to whatever link happened to be at the front of the
//! queue. A missing `bmxtranswrap.exe` is not a problem with that journalist's
//! YouTube URL, but that is what the MCR panel showed — and it showed it once
//! per job, so the pattern was buried rather than obvious.
//!
//! Checking at start-up moves every one of them to a single line in the log and
//! a single amber panel, before the first job is leased.
//!
//! **Nothing here blocks start-up.** A station with no LLM configured, or with
//! a watchfolder that is temporarily unreachable because the file server is
//! rebooting, must still come up: the queue accepts work, the panel says what
//! is wrong, and jobs fail with a specific reason instead of the daemon simply
//! not being there. Refusing to start is the one outcome an operator cannot
//! diagnose from the MCR desk.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::health::{checks, Check, Health, HealthState};
use crate::process::{self, RunOpts};

/// A tool the pipeline cannot work without.
const REQUIRED_TOOLS: &[(&str, &str)] = &[
    ("ffmpeg", "-version"),
    ("ffprobe", "-version"),
    ("bmxtranswrap", "--help"),
    ("yt-dlp", "--version"),
];

/// Result of probing one binary.
#[derive(Debug, Clone)]
pub struct ToolReport {
    pub name: String,
    pub present: bool,
    pub version: Option<String>,
    pub path: Option<PathBuf>,
}

/// Run every tool with a version flag and report what is actually there.
///
/// Existence on disk is not the check that matters: a 0-byte file left by a
/// failed download exists, and a binary built for the wrong architecture
/// exists. Running it is what proves it works.
pub async fn probe_tools(bin_dir: &Path) -> Vec<ToolReport> {
    let mut reports = Vec::new();

    for (name, flag) in REQUIRED_TOOLS {
        let path = if *name == "yt-dlp" {
            crate::dependencies::ytdl_exe_in(bin_dir)
        } else {
            bin_dir.join(format!("{name}.exe"))
        };
        if !path.exists() {
            reports.push(ToolReport {
                name: name.to_string(),
                present: false,
                version: None,
                path: None,
            });
            continue;
        }

        let version = probe_one(&path, &[flag]).await;
        reports.push(ToolReport {
            name: name.to_string(),
            present: version.is_some(),
            version,
            path: Some(path),
        });
    }

    reports
}

/// Run one binary with a version flag; `Some(first line)` when it answered.
///
/// The check is "did it produce its own output", not "did it exit zero":
/// `bmxtranswrap --help` exits non-zero by design. A binary that cannot start
/// produces neither.
pub async fn probe_one(path: &Path, args: &[&str]) -> Option<String> {
    // `RunOpts::new` discards stdout by default — it is built for the pipeline,
    // where a tool's stdout is either noise or streamed line by line. Version
    // banners go to stdout, so without this every probe reads as empty and
    // every tool is reported missing.
    let mut opts = RunOpts::new(Duration::from_secs(5));
    opts.stdout = crate::process::StdoutMode::Capture;

    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let out = process::run(path, &args, opts).await.ok()?;

    // ffmpeg writes its banner to stdout; some builds and some tools write to
    // stderr. Take whichever spoke.
    let text = if out.stdout.trim().is_empty() {
        out.stderr_tail
    } else {
        out.stdout
    };
    let first = text.lines().next().unwrap_or("").trim().to_string();
    if first.is_empty() {
        None
    } else {
        Some(first)
    }
}

/// Can we actually write to the watchfolder?
///
/// Existence and even `is_dir` are not enough: the interesting failure is a
/// share the service account can list but not write to, which is precisely what
/// running as LocalSystem against a UNC path produces (defect W-13). That
/// failure otherwise appears at the very end of the pipeline, after a correct
/// MXF has been produced, and reads as a delivery bug.
pub fn probe_watchfolder(watchfolder: &Path) -> Result<(), String> {
    if !watchfolder.exists() {
        // Try to create it: a first run on a local folder should not need the
        // operator to mkdir by hand.
        if let Err(e) = std::fs::create_dir_all(watchfolder) {
            return Err(format!("{} cannot be created: {e}", watchfolder.display()));
        }
    }
    if !watchfolder.is_dir() {
        return Err(format!("{} is not a directory", watchfolder.display()));
    }

    // A uniquely named probe, so two daemons (or a test) cannot collide, and a
    // dot prefix so a watchfolder scanner ignores it even in the window before
    // it is removed.
    let probe = watchfolder.join(format!(".omni-probe-{}.tmp", std::process::id()));
    match std::fs::write(&probe, b"omni") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(format!(
            "{} is not writable by this account: {e}",
            watchfolder.display()
        )),
    }
}

/// Free and total bytes on the volume holding `path`.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;

        // GetDiskFreeSpaceExW accepts a directory; walk up to one that exists,
        // because the watchfolder may not have been created yet.
        let mut probe = path.to_path_buf();
        while !probe.exists() {
            match probe.parent() {
                Some(p) if p != probe => probe = p.to_path_buf(),
                _ => return None,
            }
        }

        let wide: Vec<u16> = OsStr::new(&probe)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            extern "system" {
                fn GetDiskFreeSpaceExW(
                    lpDirectoryName: *const u16,
                    lpFreeBytesAvailableToCaller: *mut u64,
                    lpTotalNumberOfBytes: *mut u64,
                    lpTotalNumberOfFreeBytes: *mut u64,
                ) -> i32;
            }
            let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
            if GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) != 0 {
                return Some((free, total));
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

pub const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Minimum free space before the panel goes amber.
///
/// A single 30-minute XDCAM clip is roughly 11 GB at 50 Mbps, and a job needs
/// the source, the intermediate and the output at once. Twenty gigabytes is
/// about two jobs of headroom — enough warning to act, not so much that it
/// cries wolf on a working machine.
pub const LOW_DISK_GB: f64 = 20.0;

/// Run the whole self-test and record the results.
pub async fn run(
    health: &HealthState,
    bin_dir: &Path,
    watchfolder: &Path,
    temp_dir: &Path,
) -> Health {
    // ---- tools ----
    let tools = probe_tools(bin_dir).await;
    let missing: Vec<&str> = tools
        .iter()
        .filter(|t| !t.present)
        .map(|t| t.name.as_str())
        .collect();

    if missing.is_empty() {
        health.set(
            checks::TOOLS,
            Check::ok(format!("{} tools present", tools.len())),
        );
    } else {
        // Down, not degraded: without these, every single job fails at the
        // same step, and the newsroom needs to know that before it sends a
        // rundown rather than after.
        health.set(
            checks::TOOLS,
            Check::down(format!(
                "missing or unusable: {} (expected in {})",
                missing.join(", "),
                bin_dir.display()
            )),
        );
    }

    // ---- watchfolder ----
    match probe_watchfolder(watchfolder) {
        Ok(()) => health.set(checks::WATCHFOLDER, Check::ok("writable")),
        Err(detail) => health.set(checks::WATCHFOLDER, Check::down(detail)),
    }

    // ---- disk ----
    check_disks(health, watchfolder, temp_dir);

    health.overall()
}

/// The `accounts` check: amber while the first-run Admin / Admin still logs
/// in, so the reminder to replace it is on every desk until someone does.
/// Gone (not merely OK) once it does not: there is nothing to report then.
/// Asked at start-up and after every password or account change.
pub fn check_default_admin(repo: &crate::repository::Repository, health: &HealthState) {
    if repo.default_admin_still_works() {
        health.set_if_changed(
            checks::ACCOUNTS,
            Check::degraded(
                "Ο αρχικός λογαριασμός Admin / Admin είναι ακόμη ενεργός: δημιουργήστε δικό σας διαχειριστή \
                 (Διαχείριση → Χρήστες) και απενεργοποιήστε τον.",
            ),
        );
    } else {
        health.remove(checks::ACCOUNTS);
    }
}

/// Below this, no new job starts (plan P1.4): one more 50 Mbps file could
/// fill the disk mid-write, and a full system disk takes the database and
/// the logs down with it.
pub const STOP_DISK_GB: f64 = 5.0;

/// Free space on the temp and watchfolder volumes, recorded as the `disk`
/// check: amber under [`LOW_DISK_GB`], red under [`STOP_DISK_GB`]. Returns
/// whether there is room to start a job. Called at start-up and before
/// every lease; `set_if_changed`, so a steady state does not churn.
pub fn check_disks(health: &HealthState, watchfolder: &Path, temp_dir: &Path) -> bool {
    let mut detail = Vec::new();
    let mut state = Health::Ok;
    for (label, path) in [("temp", temp_dir), ("watchfolder", watchfolder)] {
        if let Some((free, _total)) = disk_space(path) {
            let free_gb = free as f64 / BYTES_PER_GB;
            // Whole GB, so the detail (and the check) changes when it matters.
            detail.push(format!("{label}: {free_gb:.0} GB free"));
            if free_gb < STOP_DISK_GB {
                state = state.worse(Health::Down);
            } else if free_gb < LOW_DISK_GB {
                state = state.worse(Health::Degraded);
            }
        }
    }
    let detail = if detail.is_empty() { "free space unknown".to_string() } else { detail.join(", ") };
    health.set_if_changed(
        checks::DISK,
        match state {
            Health::Ok => Check::ok(detail),
            Health::Degraded => Check::degraded(format!("low free space — {detail}")),
            Health::Down => Check::down(format!(
                "almost full — {detail}; no new job starts below {STOP_DISK_GB:.0} GB"
            )),
        },
    );
    state != Health::Down
}

/// Bytes a finished file takes per second of programme: 50 Mbps video, eight
/// 24-bit 48 kHz PCM tracks (9.2 Mbps) and MXF overhead. Measured: a 60 s
/// clip is 450 MB.
pub const MXF_BYTES_PER_SEC: f64 = 7.5e6;

/// What a job of `duration_secs` needs free before its transcode: on the
/// temp volume the intermediate and the rewrapped file at once (the
/// intermediate is deleted after the rewrap), on the watchfolder volume the
/// delivered file. Each with a 2 GB margin for everything else writing.
pub fn space_needed(duration_secs: f64) -> (u64, u64) {
    let file = (duration_secs.max(0.0) * MXF_BYTES_PER_SEC) as u64;
    const MARGIN: u64 = 2 * 1024 * 1024 * 1024;
    (2 * file + MARGIN, file + MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_clip_needs_room_for_two_copies_in_temp_and_one_in_the_watchfolder() {
        // P1.4: 60 s measured at 450 MB; temp holds the intermediate and
        // the rewrapped file at once, the watchfolder the delivered one.
        let gb = |b: u64| b as f64 / 1_073_741_824.0;
        let (temp, watch) = space_needed(60.0);
        assert!((gb(temp) - (0.838 + 2.0)).abs() < 0.01, "{}", gb(temp));
        assert!((gb(watch) - (0.419 + 2.0)).abs() < 0.01, "{}", gb(watch));
        // A 30-minute programme: ~25 GB of temp at the peak.
        assert!((gb(space_needed(1800.0).0) - 27.1).abs() < 0.2);
        assert_eq!(space_needed(-5.0), space_needed(0.0), "an unknown duration is not negative space");
    }

    #[test]
    fn the_disk_check_lets_jobs_start_when_there_is_room() {
        let d = TempDir::new().unwrap();
        let health = HealthState::new();
        // The test machine's disk has more than 5 GB free.
        assert!(check_disks(&health, d.path(), d.path()));
        let c = health.get(checks::DISK).expect("recorded");
        assert!(c.detail.unwrap_or_default().contains("temp:"));
    }

    #[test]
    fn a_writable_directory_passes_and_leaves_nothing_behind() {
        let dir = TempDir::new().unwrap();
        assert!(probe_watchfolder(dir.path()).is_ok());

        // A probe file left behind would be ingested by the watchfolder
        // scanner as if it were an asset.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftovers.is_empty(), "probe file was left behind");
    }

    #[test]
    fn a_missing_watchfolder_is_created_rather_than_failed() {
        // First run against a local path: the operator should not have to
        // mkdir by hand.
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("watch").join("nested");
        assert!(probe_watchfolder(&target).is_ok());
        assert!(target.is_dir());
    }

    #[test]
    fn a_path_that_is_a_file_is_reported_not_silently_accepted() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();

        let err = probe_watchfolder(&file).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_tool_is_reported_as_absent_rather_than_assumed_present() {
        let dir = TempDir::new().unwrap();
        let reports = probe_tools(dir.path()).await;
        assert_eq!(reports.len(), REQUIRED_TOOLS.len());
        assert!(reports.iter().all(|r| !r.present));
        assert!(reports.iter().all(|r| r.version.is_none()));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_binary_that_answers_is_detected_with_its_first_line() {
        // The check the missing-tool tests cannot make: that a *working*
        // binary is recognised. Without it, `probe_one` returning None for
        // everything looks exactly like a machine with no tools installed --
        // which is precisely the bug this test was written for, where
        // `RunOpts::new` discarded stdout and every probe read as empty.
        let version = probe_one(
            std::path::Path::new(r"C:\Windows\System32\cmd.exe"),
            &["/c", "echo", "omni-probe-ok"],
        )
        .await;

        assert_eq!(
            version.as_deref(),
            Some("omni-probe-ok"),
            "a binary that produced output on stdout was not detected"
        );
    }

    #[tokio::test]
    async fn an_empty_file_with_the_right_name_does_not_count_as_a_tool() {
        // The failure this catches: a truncated or interrupted download leaves
        // a file that `exists()` happily confirms. Only running it proves
        // anything.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("ffmpeg.exe"), b"").unwrap();

        let reports = probe_tools(dir.path()).await;
        let ffmpeg = reports.iter().find(|r| r.name == "ffmpeg").unwrap();
        assert!(!ffmpeg.present, "a 0-byte file was treated as a working tool");
    }

    #[tokio::test]
    async fn a_station_with_nothing_installed_reports_down_but_still_returns() {
        // The whole point: the self-test describes the machine, it does not
        // refuse to let it start.
        let dir = TempDir::new().unwrap();
        let health = HealthState::new();

        let verdict = run(
            &health,
            &dir.path().join("bin"),
            &dir.path().join("watch"),
            dir.path(),
        )
        .await;

        assert_eq!(verdict, Health::Down);
        assert_eq!(health.get(checks::TOOLS).unwrap().state, Health::Down);
        // The watchfolder was created, so that check passes even here.
        assert_eq!(health.get(checks::WATCHFOLDER).unwrap().state, Health::Ok);
    }
}
