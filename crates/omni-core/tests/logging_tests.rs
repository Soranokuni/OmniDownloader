//! End-to-end check that the log file is written and that a secret cannot
//! reach it (plan P6.1, defects W-11 and W-06).
//!
//! This is a separate integration binary because `tracing` allows exactly one
//! global subscriber per process: a unit test could exercise the writer, but
//! only a whole-process test proves that the layer stack actually installed
//! produces a file with the expected contents. There is therefore one test
//! here, and it asserts several things about that one run.

use std::time::Duration;

use omni_core::logging::{self, LogConfig, Redactions};
use tempfile::TempDir;

/// The value that must never appear on disk.
const SECRET: &str = "hunter2-station-mailbox";

#[test]
fn the_daemon_writes_a_rotating_log_that_never_contains_a_secret() {
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().join("logs");

    let redactions = Redactions::new(vec![SECRET.to_string()]);
    let config = LogConfig {
        filter: "info".to_string(),
        json: false,
        retention_days: 30,
    };

    // `console: false` — the test asserts on the file, and a redacted copy on
    // stderr would confuse the harness output.
    let guard = logging::init(&log_dir, &config, false, redactions.clone())
        .expect("logging should initialise");

    tracing::info!("daemon starting");

    // The realistic shape: a library logging its own failed login, and a
    // subprocess's stderr passed through verbatim. Neither goes through a
    // field we control, which is why redaction sits at the writer.
    tracing::warn!("IMAP LOGIN ingest@station.gr {SECRET} -> NO authentication failed");
    tracing::error!(
        stderr = %format!("yt-dlp: --password {SECRET} rejected"),
        "download failed"
    );

    // A structured field carrying the secret, for good measure.
    tracing::info!(password = SECRET, "configured mailbox");

    // Below the filter: proves the filter is actually applied rather than
    // everything being written.
    tracing::debug!("this line is below the configured level");

    // The appender writes on a background thread; dropping the guard flushes
    // and joins it.
    drop(guard);
    std::thread::sleep(Duration::from_millis(200));

    let files: Vec<_> = std::fs::read_dir(&log_dir)
        .expect("log directory should exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(files.len(), 1, "expected one daily log file, got {files:?}");

    let name = files[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.starts_with("omni-ingest.") && name.ends_with(".log"),
        "unexpected log file name {name}"
    );

    let contents = std::fs::read_to_string(&files[0]).unwrap();

    // 1. It actually logged. The previous build wrote to stdout only, which
    //    under the Windows service is discarded -- so the whole point is that
    //    this file exists and has the lines in it.
    assert!(contents.contains("daemon starting"), "{contents}");
    assert!(contents.contains("download failed"), "{contents}");

    // 2. The secret is absent from every one of those three routes.
    assert!(
        !contents.contains(SECRET),
        "a secret reached the log file:\n{contents}"
    );
    assert_eq!(
        contents.matches("***").count(),
        3,
        "each of the three secret occurrences should be redacted:\n{contents}"
    );

    // 3. The surrounding text survives, so the line is still diagnostic. A
    //    redactor that ate the whole line would be safe and useless.
    assert!(contents.contains("IMAP LOGIN ingest@station.gr ***"), "{contents}");
    assert!(contents.contains("authentication failed"), "{contents}");

    // 4. The filter was applied.
    assert!(
        !contents.contains("below the configured level"),
        "the configured filter was not applied:\n{contents}"
    );

    // 5. No ANSI escapes in the file. Colour codes make a log unreadable in
    //    Notepad, which is what an MCR engineer opens it with.
    assert!(
        !contents.contains('\u{1b}'),
        "the file layer emitted ANSI colour codes"
    );
}
