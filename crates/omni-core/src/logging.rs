//! Log initialisation, rotation and secret redaction (plan P6.1, defect W-11).
//!
//! Logs went to stdout and nowhere else. Under the Windows service there *is*
//! no stdout — the SCM discards it — so the moment the daemon ran the way it is
//! meant to run in the newsroom, every diagnostic it produced was thrown away.
//! The first question after any incident ("what did it say at the time?") had
//! no answer at all.
//!
//! Three things happen here:
//!
//! 1. **Files.** A daily-rotated `logs/omni-ingest.log`, kept for a bounded
//!    number of days so an unattended machine cannot fill its own disk with
//!    diagnostics and stop ingesting.
//! 2. **Redaction.** Every byte written is scanned for known secret values
//!    first. External tools quote their arguments back — yt-dlp echoes cookies,
//!    an IMAP library can log a failed `LOGIN` line verbatim — and a log file
//!    is exactly the thing that gets mailed to support.
//! 3. **Console, only when a human is watching.** `run` writes to the terminal
//!    as well; `run-service` does not, because nothing would read it.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Logging configuration (`log` in config.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    /// `tracing` filter directives, e.g. `info,omni_browser=debug`.
    #[serde(default = "default_log_filter")]
    pub filter: String,

    /// Emit JSON lines instead of human-readable text.
    ///
    /// Off by default: the reader is usually an MCR engineer with Notepad, not
    /// a log pipeline.
    #[serde(default)]
    pub json: bool,

    /// How many daily files to keep.
    #[serde(default = "default_log_retention_days")]
    pub retention_days: usize,
}

fn default_log_filter() -> String {
    // chromiumoxide is extremely chatty at info and drowns everything else.
    "info,chromiumoxide=off".to_string()
}

fn default_log_retention_days() -> usize {
    30
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: default_log_filter(),
            json: false,
            retention_days: default_log_retention_days(),
        }
    }
}

/// Values that must never appear in a log line.
///
/// Shared and swappable, because secrets are set while the daemon runs: an
/// admin who changes the mailbox password through the panel must not have the
/// new one start appearing in the log until the next restart.
#[derive(Debug, Clone, Default)]
pub struct Redactions {
    values: Arc<RwLock<Vec<String>>>,
}

impl Redactions {
    pub fn new(values: Vec<String>) -> Self {
        let me = Self::default();
        me.replace(values);
        me
    }

    /// Replace the redaction set.
    pub fn replace(&self, values: Vec<String>) {
        // Short values would match ordinary words and turn the log into a wall
        // of `***`, which is its own kind of unreadable.
        let filtered: Vec<String> = values.into_iter().filter(|v| v.len() >= 6).collect();
        if let Ok(mut guard) = self.values.write() {
            *guard = filtered;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.values.read().map(|v| v.is_empty()).unwrap_or(true)
    }

    /// Replace every known secret in `text` with `***`.
    pub fn apply(&self, text: &str) -> String {
        let Ok(values) = self.values.read() else {
            return text.to_string();
        };
        let mut out = text.to_string();
        for secret in values.iter() {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "***");
            }
        }
        out
    }
}

/// A writer that redacts before it writes.
///
/// Redaction sits at the byte level rather than at the field level on purpose:
/// a secret can reach a log through a field, through a formatted message, or
/// through a subprocess's stderr that we pass along verbatim. Only the last
/// point before the bytes hit the disk sees all three.
#[derive(Clone)]
pub struct RedactingWriter<W> {
    inner: W,
    redactions: Redactions,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.redactions.is_empty() {
            return self.inner.write(buf);
        }
        match std::str::from_utf8(buf) {
            Ok(text) => {
                let cleaned = self.redactions.apply(text);
                self.inner.write_all(cleaned.as_bytes())?;
                // Report the caller's length, not ours: redaction changes the
                // byte count, and a short write would make the fmt layer retry
                // and duplicate part of the line.
                Ok(buf.len())
            }
            Err(_) => self.inner.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// `MakeWriter` wrapper that hands out [`RedactingWriter`]s.
#[derive(Clone)]
pub struct RedactingMakeWriter<M> {
    inner: M,
    redactions: Redactions,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M, redactions: Redactions) -> Self {
        Self { inner, redactions }
    }
}

impl<'a, M> MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: MakeWriter<'a>,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
            redactions: self.redactions.clone(),
        }
    }
}

/// Keeps the background log-writing thread alive.
///
/// Dropping this flushes and stops the writer, so it has to be held for the
/// lifetime of the process. A `let _ = init(...)` would drop it immediately and
/// silently produce an empty log file.
#[must_use = "dropping the guard stops the log writer and produces an empty log file"]
pub struct LogGuard {
    _file: Option<WorkerGuard>,
    redactions: Redactions,
}

impl LogGuard {
    /// Update the redaction set after a secret changes at runtime.
    pub fn redactions(&self) -> Redactions {
        self.redactions.clone()
    }
}

/// Initialise logging. Call exactly once, as early as possible.
///
/// `console` writes to stderr as well as the file — true for `run`, false under
/// the service, where nothing reads it.
pub fn init(
    log_dir: &Path,
    config: &LogConfig,
    console: bool,
    redactions: Redactions,
) -> Result<LogGuard> {
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("Failed creating the log directory {log_dir:?}"))?;

    // `RUST_LOG` wins when set, so an engineer debugging on the box does not
    // have to edit config.json and restart twice.
    let filter = |cfg: &str| {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(cfg.to_string()))
    };

    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("omni-ingest")
        .filename_suffix("log")
        .max_log_files(config.retention_days.clamp(1, 365))
        .build(log_dir)
        .with_context(|| format!("Failed creating the rolling log appender in {log_dir:?}"))?;

    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let file_writer = RedactingMakeWriter::new(file_writer, redactions.clone());

    let registry = tracing_subscriber::registry();

    // Two near-identical arms because the JSON and text formatters are
    // different types; boxing them costs more clarity than it saves.
    if config.json {
        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(file_writer)
            .with_ansi(false)
            .with_filter(filter(&config.filter));

        let console_layer = console.then(|| {
            tracing_subscriber::fmt::layer()
                .with_writer(RedactingMakeWriter::new(
                    std::io::stderr,
                    redactions.clone(),
                ))
                .with_ansi(true)
                .with_filter(filter(&config.filter))
        });

        registry.with(file_layer).with(console_layer).init();
    } else {
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(file_writer)
            .with_ansi(false)
            .with_target(true)
            .with_filter(filter(&config.filter));

        let console_layer = console.then(|| {
            tracing_subscriber::fmt::layer()
                .with_writer(RedactingMakeWriter::new(
                    std::io::stderr,
                    redactions.clone(),
                ))
                .with_ansi(true)
                .with_filter(filter(&config.filter))
        });

        registry.with(file_layer).with(console_layer).init();
    }

    Ok(LogGuard {
        _file: Some(guard),
        redactions,
    })
}

/// Minimal console-only logging, for the short-lived CLI subcommands.
///
/// `secrets list`, `adblock status` and friends should not open, rotate or
/// prune the log file the running service is writing to.
pub fn init_console_only(filter: &str) {
    let env = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(filter.to_string()));
    let _ = tracing_subscriber::fmt().with_env_filter(env).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Collects what a `RedactingWriter` actually emitted.
    #[derive(Clone, Default)]
    struct Sink(Arc<RwLock<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8(self.0.read().unwrap().clone()).unwrap()
        }
    }

    fn writer(sink: Sink, secrets: &[&str]) -> RedactingWriter<Sink> {
        RedactingWriter {
            inner: sink,
            redactions: Redactions::new(secrets.iter().map(|s| s.to_string()).collect()),
        }
    }

    #[test]
    fn a_secret_never_reaches_the_sink() {
        let sink = Sink::default();
        let mut w = writer(sink.clone(), &["hunter2-mailbox-pass"]);

        // The shape an IMAP library actually logs on a failed login.
        write!(w, "IMAP LOGIN ingest@station.gr hunter2-mailbox-pass -> NO").unwrap();

        let out = sink.text();
        assert!(!out.contains("hunter2-mailbox-pass"), "{out}");
        assert_eq!(out, "IMAP LOGIN ingest@station.gr *** -> NO");
    }

    #[test]
    fn the_caller_is_told_it_wrote_everything_it_asked_to() {
        // Redaction shortens the output. Returning the *written* length would
        // make the fmt layer treat it as a short write and re-send the tail,
        // duplicating part of every redacted line.
        let sink = Sink::default();
        let mut w = writer(sink.clone(), &["a-long-secret-value"]);
        let payload = b"prefix a-long-secret-value suffix";
        assert_eq!(w.write(payload).unwrap(), payload.len());
        assert_eq!(sink.text(), "prefix *** suffix");
    }

    #[test]
    fn a_secret_split_across_two_writes_is_the_known_gap() {
        // Honest about the limitation: redaction is per write call, and
        // `tracing` emits a whole event per call, so this does not arise in
        // practice -- but a future caller streaming bytes should know.
        let sink = Sink::default();
        let mut w = writer(sink.clone(), &["secret-value-here"]);
        w.write_all(b"secret-").unwrap();
        w.write_all(b"value-here").unwrap();
        assert_eq!(sink.text(), "secret-value-here");
    }

    #[test]
    fn short_values_are_not_redacted() {
        // Redacting "abc" would black out ordinary words and make the log
        // useless, which is a failure too.
        let sink = Sink::default();
        let mut w = writer(sink.clone(), &["abc"]);
        write!(w, "abc appears inside abcdef").unwrap();
        assert_eq!(sink.text(), "abc appears inside abcdef");
    }

    #[test]
    fn with_no_secrets_the_bytes_pass_through_untouched() {
        let sink = Sink::default();
        let mut w = writer(sink.clone(), &[]);
        write!(w, "ordinary line").unwrap();
        assert_eq!(sink.text(), "ordinary line");
    }

    #[test]
    fn non_utf8_bytes_are_passed_through_rather_than_dropped() {
        let sink = Sink::default();
        let mut w = writer(sink.clone(), &["a-long-secret-value"]);
        w.write_all(&[0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(sink.0.read().unwrap().as_slice(), &[0xff, 0xfe, 0x00]);
    }

    #[test]
    fn the_redaction_set_can_be_replaced_while_running() {
        // An admin changing the mailbox password through the panel must not
        // have the new value start appearing in the log until a restart.
        let sink = Sink::default();
        let redactions = Redactions::new(vec!["old-secret-value".into()]);
        let mut w = RedactingWriter {
            inner: sink.clone(),
            redactions: redactions.clone(),
        };

        write!(w, "[new-secret-value]").unwrap();
        assert_eq!(sink.text(), "[new-secret-value]");

        redactions.replace(vec!["new-secret-value".into()]);
        write!(w, "[new-secret-value]").unwrap();
        assert!(sink.text().ends_with("[***]"), "{}", sink.text());
    }

    #[test]
    fn the_default_filter_silences_the_browser_driver() {
        // chromiumoxide at info drowns every other line in the file, which is
        // how a 30-day retention turns into three days of useful history.
        assert!(LogConfig::default().filter.contains("chromiumoxide=off"));
    }
}
