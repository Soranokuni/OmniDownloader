//! The diagnostics package (P13.8): one zip an administrator can send to
//! support, with nothing secret in it.
//!
//! In it: the logs of the last 7 days, the configuration, the health checks,
//! the tool versions and the last failed jobs with their history. Every text
//! goes through [`scrub`] first: the known secret values (the encrypted
//! store's and the cookie jars') become `***`, a web address loses its user
//! name, password, query and fragment, and cookie, authorization and session
//! headers lose their values. The configuration also loses every
//! secret-named value ([`crate::config_v2::without_secrets`]).
//!
//! Never in it: `data\secrets.bin`, cookie jars, the database, session
//! tokens. Nothing is read from those files at all; the zip is built from
//! the log folder, config.json and what the caller passes.
//!
//! Size-bound: each log file contributes at most its last
//! [`Limits::per_log_bytes`], all logs together at most
//! [`Limits::total_log_bytes`], newest first.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;

use crate::models::{Job, JobEvent};

/// How much goes in.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub log_days: i64,
    pub per_log_bytes: u64,
    pub total_log_bytes: u64,
    pub failed_jobs: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { log_days: 7, per_log_bytes: 8 * 1024 * 1024, total_log_bytes: 40 * 1024 * 1024, failed_jobs: 50 }
    }
}

/// What the caller gathers from the running daemon.
pub struct Sources {
    pub logs_dir: PathBuf,
    pub config_path: PathBuf,
    /// `HealthState::all()` as JSON.
    pub health: Value,
    /// The toolchain rows as the admin panel shows them.
    pub tools: Value,
    /// The last failed jobs, newest first, each with its events.
    pub failed: Vec<(Job, Vec<JobEvent>)>,
    /// Every secret value known: the store's, the cookie jars', the log redactor's.
    pub secrets: Vec<String>,
    pub version: String,
    pub now: DateTime<Utc>,
}

/// What went into a package.
#[derive(Debug, Clone, Default)]
pub struct Written {
    pub logs: Vec<String>,
    pub log_bytes: u64,
    /// A log file that was cut to its last part.
    pub truncated: Vec<String>,
    pub failed_jobs: usize,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("a valid pattern"))
}

/// `text` with nothing secret left in it (see the module text).
pub fn scrub(text: &str, secrets: &[String]) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    static HEADER: OnceLock<Regex> = OnceLock::new();
    static SESSION: OnceLock<Regex> = OnceLock::new();
    let text = crate::secrets::redact(text, secrets);
    // scheme :// [userinfo@] rest [?query] [#fragment]
    let url = re(&URL, r#"(?i)\b([a-z][a-z0-9+.-]*)://(?:[^\s/?#"'<>@]*@)?([^\s?#"'<>]*)(\?[^\s#"'<>]*)?(#[^\s"'<>]*)?"#);
    let text = url.replace_all(&text, |c: &regex::Captures| {
        let q = if c.get(3).is_some() || c.get(4).is_some() { "?…" } else { "" };
        format!("{}://{}{q}", &c[1], &c[2])
    });
    let header = re(&HEADER, r#"(?i)\b(set-cookie|cookie|authorization|proxy-authorization|x-api-key|api-key)(\s*[:=]\s*)[^\r\n]*"#);
    let text = header.replace_all(&text, "$1$2<removed>");
    let session = re(&SESSION, r#"(?i)\b(omni_session)=[^;\s"']*"#);
    session.replace_all(&text, "$1=<removed>").into_owned()
}

/// Every string in `doc`, scrubbed.
fn scrub_json(doc: &Value, secrets: &[String]) -> Value {
    match doc {
        Value::String(s) => Value::String(scrub(s, secrets)),
        Value::Array(a) => Value::Array(a.iter().map(|v| scrub_json(v, secrets)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), scrub_json(v, secrets))).collect()),
        other => other.clone(),
    }
}

/// The daemon's log files of the last `days`, newest first.
fn recent_logs(dir: &Path, days: i64, now: DateTime<Utc>) -> Vec<(PathBuf, std::time::SystemTime)> {
    let cutoff = std::time::SystemTime::from(now - chrono::Duration::days(days));
    let mut out: Vec<(PathBuf, std::time::SystemTime)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_ascii_lowercase();
            n.starts_with("omni-ingest") && n.ends_with(".log")
        })
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            let modified = md.modified().ok()?;
            (md.is_file() && modified >= cutoff).then(|| (e.path(), modified))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// The last `max` bytes of `path`, from the first whole line.
fn tail(path: &Path, max: u64) -> Result<(String, bool)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let cut = len > max;
    if cut {
        f.seek(SeekFrom::Start(len - max))?;
    }
    let mut bytes = Vec::with_capacity(len.min(max) as usize);
    f.take(max).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if cut {
        if let Some(i) = text.find('\n') {
            text.drain(..=i);
        }
    }
    Ok((text, cut))
}

/// config.json as the package holds it: secret-named values blanked, every
/// string scrubbed. Unreadable: a note instead.
fn config_copy(path: &Path, secrets: &[String]) -> String {
    let doc = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    match doc {
        Some(d) => serde_json::to_string_pretty(&scrub_json(&crate::config_v2::without_secrets(&d), secrets)).unwrap_or_default(),
        None => "config.json could not be read as JSON; it is not included.".into(),
    }
}

fn failed_json(failed: &[(Job, Vec<JobEvent>)], secrets: &[String]) -> Value {
    let jobs: Vec<Value> = failed
        .iter()
        .map(|(j, events)| {
            serde_json::json!({
                "id": j.id,
                "url": j.url,
                "slug": j.slug,
                "journalist": j.journalist,
                "status": j.status.as_str(),
                "stage": j.stage.as_str(),
                "error_code": j.error_code,
                "error_message": j.error_message,
                "attempts": j.attempts,
                "extraction_method": j.extraction_method,
                "completed_at": j.completed_at.map(|t| t.to_rfc3339()),
                "events": events.iter().map(|e| serde_json::json!({
                    "at": e.at.map(|t| t.to_rfc3339()),
                    "level": e.level,
                    "stage": e.stage,
                    "message": e.message,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    scrub_json(&Value::Array(jobs), secrets)
}

/// Write the package to `out` (a new file). See the module text for what
/// is in it.
pub fn write_zip(out: &Path, src: &Sources, limits: Limits) -> Result<Written> {
    let file = std::fs::File::create(out).with_context(|| format!("Cannot create {}", out.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let put = |zip: &mut zip::ZipWriter<std::fs::File>, name: &str, body: &str| -> Result<()> {
        zip.start_file(name, opts)?;
        zip.write_all(body.as_bytes())?;
        Ok(())
    };
    let mut written = Written::default();

    let mut budget = limits.total_log_bytes;
    for (path, _) in recent_logs(&src.logs_dir, limits.log_days, src.now) {
        if budget == 0 {
            break;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (text, cut) = match tail(&path, limits.per_log_bytes.min(budget)) {
            Ok(t) => t,
            Err(e) => (format!("(this log could not be read: {e})\n"), false),
        };
        let text = scrub(&text, &src.secrets);
        budget = budget.saturating_sub(text.len() as u64);
        written.log_bytes += text.len() as u64;
        if cut {
            written.truncated.push(name.clone());
        }
        put(&mut zip, &format!("logs/{name}"), &text)?;
        written.logs.push(name);
    }

    put(&mut zip, "config.json", &config_copy(&src.config_path, &src.secrets))?;
    put(&mut zip, "health.json", &serde_json::to_string_pretty(&scrub_json(&src.health, &src.secrets))?)?;
    put(&mut zip, "tools.json", &serde_json::to_string_pretty(&scrub_json(&src.tools, &src.secrets))?)?;
    let failed = &src.failed[..src.failed.len().min(limits.failed_jobs)];
    written.failed_jobs = failed.len();
    put(&mut zip, "failed_jobs.json", &serde_json::to_string_pretty(&failed_json(failed, &src.secrets))?)?;

    let readme = format!(
        "OmniIngest diagnostics package\r\n\
         Made {} by version {}.\r\n\r\n\
         logs/              the service log of the last {} days ({} file(s){})\r\n\
         config.json        the settings; passwords, secrets and keys are <removed>\r\n\
         health.json        the health checks as the admin panel showed them\r\n\
         tools.json         the tools and their versions\r\n\
         failed_jobs.json   the last {} failed job(s) and their history\r\n\r\n\
         Known secret values are written as ***, web addresses without user name,\r\n\
         password or query (?…), cookie and authorization headers as <removed>.\r\n\
         Not included: the encrypted secret store, cookie jars, the database, sessions.\r\n",
        src.now.format("%Y-%m-%d %H:%M UTC"),
        src.version,
        limits.log_days,
        written.logs.len(),
        if written.truncated.is_empty() { String::new() } else { format!("; only the last part of {}", written.truncated.join(", ")) },
        written.failed_jobs,
    );
    put(&mut zip, "README.txt", &readme)?;
    zip.finish()?;
    Ok(written)
}

/// Remove all but the newest `keep` packages in `dir` (only files this
/// module names `omni-diagnostics-*.zip`).
pub fn prune(dir: &Path, keep: usize) {
    let mut zips: Vec<(PathBuf, std::time::SystemTime)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("omni-diagnostics-") && n.ends_with(".zip")
        })
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    zips.sort_by(|a, b| b.1.cmp(&a.1));
    for (path, _) in zips.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{JobStatus, NewJob};
    use crate::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

    const GRAPH: &str = "graph-secret-planted-123";
    const JAR: &str = "jarvalue-planted-xyz";
    /// Strings that must not be anywhere in the package.
    const PLANTED: &[&str] = &[
        GRAPH, JAR, "pw-planted", "query-planted-789", "session-planted-abc", "cookie-planted-456",
        "sk-planted-key", "imap-planted-pass", "bearer-planted-tok", "OLD-LOG-LINE", "dpapi-file-content",
    ];

    fn unzip_all(path: &Path) -> Vec<(String, String)> {
        let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        (0..z.len())
            .map(|i| {
                let mut f = z.by_index(i).unwrap();
                let mut s = String::new();
                f.read_to_string(&mut s).unwrap();
                (f.name().to_string(), s)
            })
            .collect()
    }

    /// An installation with secrets planted everywhere a package reads.
    fn planted() -> (tempfile::TempDir, Sources) {
        let dir = tempfile::TempDir::new().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("omni-ingest.2026-11-20.log"),
            format!(
                "INFO job 7 started\nWARN token {GRAPH} rejected\nINFO yt-dlp https://user:pw-planted@cdn.example/v.mp4?token=query-planted-789&x=1 done\n\
                 DEBUG Cookie: SID=cookie-planted-456; other=1\nDEBUG authorization: Bearer bearer-planted-tok\nINFO omni_session=session-planted-abc seen\nINFO jar {JAR}\n"
            ),
        )
        .unwrap();
        let old = logs.join("omni-ingest.2026-11-01.log");
        std::fs::write(&old, "OLD-LOG-LINE\n").unwrap();
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 86_400)).unwrap();
        std::fs::write(logs.join("secrets.bin"), "dpapi-file-content").unwrap();
        let config = dir.path().join("config.json");
        std::fs::write(
            &config,
            r#"{ "web_port": 8081, "email_password": "imap-planted-pass", "llm": { "endpoint": "https://user:pw-planted@llm.example/v1?key=query-planted-789", "api_key": "sk-planted-key", "max_tokens": 400 },
                 "graph": { "mailbox": "ingest@station.example", "client_secret": "graph-secret-planted-123" } }"#,
        )
        .unwrap();

        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let id = repo
            .enqueue(&NewJob::new("https://site.example/a?sig=query-planted-789", "1_TESTAKIS_FAIL", "TESTAKIS"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();
        assert_eq!(repo.lease_job("diag-test:1:1", 600).unwrap().unwrap().id, id);
        repo.record_event(id, "ERROR", None, &format!("yt-dlp said Cookie: {JAR} and {GRAPH}")).unwrap();
        assert!(repo.finish(id, "diag-test:1:1", JobStatus::Failed, Some("DOWNLOAD_FAILED"), Some("HTTP 403 for https://cdn.example/x?token=query-planted-789"), None).unwrap());
        let failed = repo.recent_failed_jobs(50).unwrap().into_iter().map(|j| {
            let ev = repo.recent_job_events(j.id, 100).unwrap();
            (j, ev)
        }).collect();

        let src = Sources {
            logs_dir: logs,
            config_path: config,
            health: serde_json::json!({ "mail": { "state": "down", "detail": format!("login failed with {GRAPH}") } }),
            tools: serde_json::json!([{ "name": "yt-dlp", "version": "2026.11.01" }]),
            failed,
            secrets: vec![GRAPH.to_string(), JAR.to_string()],
            version: "1.0.0".into(),
            now: Utc::now(),
        };
        (dir, src)
    }

    #[test]
    fn a_package_holds_the_logs_config_health_tools_and_failures_and_no_planted_secret() {
        let (dir, src) = planted();
        let out = dir.path().join("omni-diagnostics-test.zip");
        let w = write_zip(&out, &src, Limits::default()).unwrap();
        assert_eq!(w.logs, ["omni-ingest.2026-11-20.log"], "the last 7 days, the daemon's logs only");
        assert_eq!(w.failed_jobs, 1);

        let files = unzip_all(&out);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["logs/omni-ingest.2026-11-20.log", "config.json", "health.json", "tools.json", "failed_jobs.json", "README.txt"]);
        let all: String = files.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n");
        for p in PLANTED {
            assert!(!all.contains(p), "{p} is in the package:\n{all}");
        }
        // What support needs is still there.
        let log = &files[0].1;
        assert!(log.contains("INFO job 7 started") && log.contains("WARN token *** rejected"), "{log}");
        assert!(log.contains("https://cdn.example/v.mp4?…"), "{log}");
        let config: Value = serde_json::from_str(&files[1].1).unwrap();
        assert_eq!((config["web_port"].as_u64(), config["llm"]["max_tokens"].as_u64()), (Some(8081), Some(400)));
        assert_eq!(config["llm"]["endpoint"], "https://llm.example/v1?…");
        assert!(files[4].1.contains("DOWNLOAD_FAILED") && files[4].1.contains("https://site.example/a?…"), "{}", files[4].1);
    }

    #[test]
    fn the_logs_are_size_bound_and_cut_at_a_line() {
        let (dir, src) = planted();
        let big = src.logs_dir.join("omni-ingest.2026-11-21.log");
        let line = "INFO a line that repeats to make the log large\n";
        std::fs::write(&big, line.repeat(60_000)).unwrap();
        let limits = Limits { per_log_bytes: 100_000, total_log_bytes: 120_000, ..Limits::default() };
        let out = dir.path().join("omni-diagnostics-big.zip");
        let w = write_zip(&out, &src, limits).unwrap();
        assert!(w.log_bytes <= 120_000, "{}", w.log_bytes);
        assert!(w.truncated.contains(&"omni-ingest.2026-11-21.log".to_string()), "{w:?}");
        let files = unzip_all(&out);
        let big_text = &files.iter().find(|(n, _)| n.ends_with("2026-11-21.log")).unwrap().1;
        assert!(big_text.len() <= 100_000 && big_text.starts_with("INFO a line"), "cut at a whole line");
        assert!(files.iter().any(|(n, s)| n == "README.txt" && s.contains("only the last part")));
    }

    #[test]
    fn only_the_newest_packages_are_kept() {
        let dir = tempfile::TempDir::new().unwrap();
        for n in 0..5 {
            let p = dir.path().join(format!("omni-diagnostics-{n}.zip"));
            std::fs::write(&p, b"x").unwrap();
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(100 - n * 10)).unwrap();
        }
        std::fs::write(dir.path().join("other.zip"), b"x").unwrap();
        prune(dir.path(), 3);
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["omni-diagnostics-2.zip", "omni-diagnostics-3.zip", "omni-diagnostics-4.zip", "other.zip"]);
    }
}
