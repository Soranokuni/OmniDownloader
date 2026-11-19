//! What Setup said, kept in `<install folder>\setup.log`: every line is
//! collected in memory and appended there at the end, and nowhere else. The
//! install folder itself is for administrators only; `logs\` is not (the
//! service account writes it, and could put a link there to steer where an
//! elevated process writes). A password never reaches a line: none is ever
//! passed to one.

use std::io::Write;
use std::path::Path;

use omni_cli::install::{Progress, Step};

/// Collects every line; prints it too unless quiet.
#[derive(Debug, Default)]
pub struct SetupLog {
    pub lines: Vec<String>,
    pub echo: bool,
}

impl SetupLog {
    pub fn new(echo: bool) -> Self {
        SetupLog { lines: Vec::new(), echo }
    }

    /// A line of Setup's own.
    pub fn say(&mut self, text: &str) {
        self.line(Step::Plan, text);
    }
}

impl Progress for SetupLog {
    fn line(&mut self, _step: Step, text: &str) {
        if self.echo {
            println!("{text}");
        }
        self.lines.push(text.to_string());
    }
}

/// Days since 1970-01-01 as (year, month, day), proleptic Gregorian.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// `yyyymmdd` (UTC) for the Apps & Features `InstallDate`.
pub fn yyyymmdd(unix_secs: i64) -> String {
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    format!("{y:04}{m:02}{d:02}")
}

/// RFC 3339 in UTC, the project's timestamp form.
pub fn rfc3339(unix_secs: i64) -> String {
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    let s = unix_secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The log's file name in the install folder.
pub const SETUP_LOG: &str = "setup.log";

/// Append `lines` under a header to `<root>\setup.log`, only when the install
/// folder exists (and the log is not a link). `Ok(false)` when nothing was written.
pub fn append_setup_log(root: &Path, header: &str, lines: &[String], unix_secs: i64) -> std::io::Result<bool> {
    let log = root.join(SETUP_LOG);
    if !root.is_dir() || omni_core::fsutil::is_link(root) || omni_core::fsutil::is_link(&log) {
        return Ok(false);
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let mut text = format!("\r\n==== {} {header} ====\r\n", rfc3339(unix_secs));
    for l in lines {
        text.push_str(l);
        text.push_str("\r\n");
    }
    f.write_all(text.as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(yyyymmdd(0), "19700101");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        // 2026-11-17T13:45:30Z
        assert_eq!(rfc3339(1_794_923_130), "2026-11-17T13:45:30Z");
        assert_eq!(yyyymmdd(1_794_923_130), "20261117");
        // A leap day.
        assert_eq!(yyyymmdd(951_782_400), "20000229");
    }

    #[test]
    fn the_log_goes_into_the_install_folder_itself_never_into_logs() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Omni");
        let lines = vec!["one".to_string(), "two".to_string()];
        assert!(!append_setup_log(&root, "Setup 1.0.0 install", &lines, 0).unwrap());
        assert!(!root.exists(), "no folder is created for the log");
        std::fs::create_dir_all(root.join("logs")).unwrap();
        assert!(append_setup_log(&root, "Setup 1.0.0 install", &lines, 0).unwrap());
        assert!(append_setup_log(&root, "Setup 1.0.0 uninstall", &lines, 60).unwrap());
        assert!(!root.join("logs").join("setup.log").exists(), "logs\\ is the service account's");
        let text = std::fs::read_to_string(root.join("setup.log")).unwrap();
        assert!(text.contains("==== 1970-01-01T00:00:00Z Setup 1.0.0 install ====\r\none\r\ntwo\r\n"), "{text}");
        assert!(text.contains("1970-01-01T00:01:00Z Setup 1.0.0 uninstall"), "appended, not replaced");
    }

    #[test]
    fn collected_lines_are_kept_in_order_without_printing_when_quiet() {
        let mut log = SetupLog::new(false);
        log.say("a");
        log.line(Step::Health, "b");
        assert_eq!(log.lines, ["a", "b"]);
    }
}
