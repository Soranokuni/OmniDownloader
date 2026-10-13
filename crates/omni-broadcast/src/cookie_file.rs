//! The cookie jar as a file for yt-dlp (plan P3.5).
//!
//! The jar is stored sealed; yt-dlp can only read a file. The file exists for
//! the length of one yt-dlp run and is removed when its guard drops, on
//! success, error, cancel and an aborted task alike. Cookie content is never
//! logged or put in an error here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use omni_core::cookies::{host_and_path, jar_domain_for_url, scrub_detail, validate_sample_url};
use omni_core::repository::Repository;
use tracing::warn;

/// Name of the file inside the job's workspace.
pub const COOKIE_FILE_NAME: &str = "cookies.txt";

/// A jar written to disk for one yt-dlp run; removed on drop.
pub struct CookieFile {
    path: PathBuf,
    /// A directory made for this file alone, removed with it.
    own_dir: Option<PathBuf>,
    /// The jar's values, to scrub yt-dlp's error text, which can quote a line.
    values: Vec<String>,
}

impl CookieFile {
    /// Write `text` as `dir/cookies.txt`. `dir` is the job's workspace. Never
    /// follows a link or replaces anything but an earlier run's plain file.
    pub fn write(dir: &Path, text: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(COOKIE_FILE_NAME);
        // A leftover of a crashed run is ours to replace; anything else
        // (a link, a directory) makes create_new fail below.
        if path.is_file() {
            std::fs::remove_file(&path)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
        // From here the file exists, so the guard owns its removal.
        let guard = Self { path, own_dir: None, values: omni_core::cookies::values_of(text) };
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        Ok(guard)
    }

    /// Write `text` into a new directory under `base/cookies/`, for calls that
    /// have no job workspace (the page scan, the test). The directory goes
    /// with the guard.
    pub fn write_scratch(base: &Path, text: &str) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = base.join("cookies").join(format!("{}-{stamp}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut file = Self::write(&dir, text).map_err(|e| {
            let _ = std::fs::remove_dir_all(&dir);
            e
        })?;
        file.own_dir = Some(dir);
        Ok(file)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every cookie value in the file. Never log or return these.
    pub fn values(&self) -> &[String] {
        &self.values
    }
}

/// Remove the scratch jars under `temp/cookies/` (plan P3.5). A crash can leave
/// one; nothing is using them at start-up, before the web server and workers run.
pub fn sweep_scratch(temp_dir: &Path) {
    let dir = temp_dir.join("cookies");
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            warn!("Could not remove the leftover cookie files in {dir:?}: {e}");
        }
    }
}

impl Drop for CookieFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(dir) = &self.own_dir {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// The jar for the site of `url`, written for one run. `None` when the site
/// has no jar, or the jar cannot be opened (logged without its content): the
/// download then runs as before and fails the way it would have. `scratch`
/// writes into a directory of its own under `dir` (see [`CookieFile::write_scratch`]).
pub fn for_url(repo: &Repository, url: &str, dir: &Path, scratch: bool) -> Option<CookieFile> {
    for_urls(repo, &[url], dir, scratch)
}

/// One file holding the jars of every site in `urls` (each once). yt-dlp sends a
/// cookie only to its own domain, so a job on a news article that embeds an X
/// post can carry both sites' jars in one file.
pub fn for_urls(repo: &Repository, urls: &[&str], dir: &Path, scratch: bool) -> Option<CookieFile> {
    let mut domains: Vec<String> = Vec::new();
    for u in urls {
        if let Some(d) = jar_domain_for_url(u) {
            if !domains.contains(&d) {
                domains.push(d);
            }
        }
    }
    let mut text = String::new();
    for domain in &domains {
        match repo.cookie_jar_text(domain) {
            Ok(Some(t)) => {
                text.push_str(&t);
                if !text.ends_with('\n') {
                    text.push('\n');
                }
            }
            Ok(None) => {}
            Err(e) => warn!("The cookie jar for {domain} could not be opened: {e:#}"),
        }
    }
    if text.is_empty() {
        return None;
    }
    let domain = domains.join("+");
    let written = if scratch { CookieFile::write_scratch(dir, &text) } else { CookieFile::write(dir, &text) };
    match written {
        Ok(f) => Some(f),
        Err(e) => {
            warn!("The cookie jar for {domain} could not be written for yt-dlp: {e}");
            None
        }
    }
}

/// The yt-dlp arguments of a jar test: no download, one video's metadata.
pub fn test_args(cookies: &Path, url: &str) -> Vec<String> {
    [
        "--cookies",
        &cookies.to_string_lossy(),
        "--skip-download",
        "--dump-json",
        "--no-playlist",
        "--playlist-items",
        "1",
        "--no-warnings",
        "--socket-timeout",
        "15",
        url,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The result of one jar test, as stored: `ok`, or the first error line with no query strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JarTest {
    pub ok: bool,
    pub detail: String,
}

/// Test the jar for `domain` against `url` (`yt-dlp --cookies ... --skip-download
/// --dump-json`), store the outcome with `url` as the nightly sample, and return it.
/// `Err` only when the test could not be set up (no jar, bad link, cannot write).
pub async fn test_jar(ytdl: &Path, temp_dir: &Path, repo: &Repository, domain: &str, url: &str) -> Result<JarTest, String> {
    let url = validate_sample_url(domain, url).map_err(|e| e.to_string())?;
    let text = match repo.cookie_jar_text(domain) {
        Ok(Some(t)) => t,
        Ok(None) => return Err("There is no cookie jar for this site.".to_string()),
        Err(e) => return Err(format!("The cookie jar could not be opened: {e:#}")),
    };
    let values: Vec<String> = repo.cookie_values();
    let file = CookieFile::write_scratch(temp_dir, &text).map_err(|e| format!("Could not prepare the test: {e}"))?;
    let args = test_args(file.path(), &url);
    let outcome = omni_core::process::run_capture(ytdl, args, Duration::from_secs(90)).await;
    drop(file);

    let test = match outcome {
        Ok(o) if o.success && !o.stdout.trim().is_empty() => JarTest { ok: true, detail: "ok".to_string() },
        Ok(o) if o.success => JarTest { ok: false, detail: "yt-dlp found no video".to_string() },
        Ok(o) if o.timed_out => JarTest { ok: false, detail: "yt-dlp did not answer within 90 s".to_string() },
        Ok(o) => JarTest { ok: false, detail: scrub_detail(&crate::downloader::first_error_line(&o.stderr_tail), &values) },
        Err(e) => JarTest { ok: false, detail: scrub_detail(&format!("could not run yt-dlp: {e:#}"), &values) },
    };
    if let Err(e) = repo.record_cookie_test(domain, test.ok, &test.detail, &url, chrono::Utc::now()) {
        warn!("The result of the cookie test for {domain} was not stored: {e:#}");
    }
    tracing::info!("Cookie test for {domain} ({}): {}", host_and_path(&url), if test.ok { "ok" } else { "failed" });
    Ok(test)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_up_sweep_removes_leftover_scratch_jars_and_only_those() {
        let dir = tempfile::tempdir().unwrap();
        let a = CookieFile::write_scratch(dir.path(), TEXT).unwrap();
        let left = a.path().to_path_buf();
        std::mem::forget(a); // a crash: the guard never runs
        assert!(left.exists());
        let job = dir.path().join("jobs").join("3");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(job.join("source.mp4"), b"x").unwrap();

        sweep_scratch(dir.path());
        assert!(!left.exists() && !dir.path().join("cookies").exists());
        assert!(job.join("source.mp4").exists(), "a job's workspace is not touched");
        sweep_scratch(dir.path()); // nothing there: no error
    }

    #[test]
    fn a_file_knows_its_values() {
        let dir = tempfile::tempdir().unwrap();
        let f = CookieFile::write(dir.path(), TEXT).unwrap();
        assert_eq!(f.values(), ["file-value-123456".to_string()]);
    }

    const TEXT: &str = ".x.com\tTRUE\t/\tTRUE\t0\tsid\tfile-value-123456\n";

    #[test]
    fn the_file_is_written_and_removed_when_the_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let job = dir.path().join("jobs").join("7");
        let f = CookieFile::write(&job, TEXT).unwrap();
        let p = f.path().to_path_buf();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), TEXT);
        assert_eq!(p, job.join("cookies.txt"));
        drop(f);
        assert!(!p.exists(), "the jar must not outlive the run");
    }

    #[test]
    fn the_file_is_removed_on_an_early_return_and_on_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        fn fails(dir: &Path) -> Result<(), ()> {
            let _jar = CookieFile::write(dir, TEXT).unwrap();
            Err(())
        }
        assert!(fails(dir.path()).is_err());
        assert!(!dir.path().join("cookies.txt").exists());

        let p = dir.path().to_path_buf();
        let r = std::panic::catch_unwind(move || {
            let _jar = CookieFile::write(&p, TEXT).unwrap();
            panic!("worker died");
        });
        assert!(r.is_err());
        assert!(!dir.path().join("cookies.txt").exists());
    }

    #[test]
    fn the_file_is_removed_when_the_task_holding_it_is_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let p = path.clone();
            let task = tokio::spawn(async move {
                let _jar = CookieFile::write(&p, TEXT).unwrap();
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(path.join("cookies.txt").exists());
            task.abort();
            let _ = task.await;
        });
        assert!(!path.join("cookies.txt").exists(), "a cancelled job left its jar behind");
    }

    #[test]
    fn a_scratch_jar_takes_its_directory_with_it_and_two_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let a = CookieFile::write_scratch(dir.path(), TEXT).unwrap();
        let b = CookieFile::write_scratch(dir.path(), TEXT).unwrap();
        assert_ne!(a.path(), b.path());
        let (pa, pb) = (a.path().to_path_buf(), b.path().to_path_buf());
        drop(a);
        assert!(!pa.exists() && !pa.parent().unwrap().exists());
        assert!(pb.exists(), "dropping one jar must not touch another's");
        drop(b);
        assert_eq!(std::fs::read_dir(dir.path().join("cookies")).unwrap().count(), 0);
    }

    #[test]
    fn a_jar_exists_only_for_a_site_that_has_one() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let jar = omni_core::cookies::parse_netscape("x.com", TEXT).unwrap();
        repo.put_cookie_jar("x.com", &jar, "admin", chrono::Utc::now()).unwrap();
        let work = dir.path().join("jobs").join("1");

        // twitter.com is served by the x.com jar; another site gets none.
        let f = for_url(&repo, "https://twitter.com/a/status/1", &work, false).expect("alias");
        assert!(std::fs::read_to_string(f.path()).unwrap().contains("file-value-123456"));
        drop(f);
        assert!(for_url(&repo, "https://www.example.gr/video", &work, false).is_none());
        assert!(!work.join("cookies.txt").exists());
    }

    #[test]
    fn an_article_that_embeds_another_site_carries_both_jars_in_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let x = omni_core::cookies::parse_netscape("x.com", TEXT).unwrap();
        repo.put_cookie_jar("x.com", &x, "admin", chrono::Utc::now()).unwrap();
        let news = omni_core::cookies::parse_netscape("news.example", ".news.example\tTRUE\t/\tTRUE\t0\tsid\tnews-value-123456\n").unwrap();
        repo.put_cookie_jar("news.example", &news, "admin", chrono::Utc::now()).unwrap();
        let work = dir.path().join("jobs").join("2");

        let f = for_urls(&repo, &["https://www.news.example/story/1", "https://x.com/a/status/1"], &work, false).unwrap();
        let body = std::fs::read_to_string(f.path()).unwrap();
        assert!(body.contains("file-value-123456") && body.contains("news-value-123456"));
        assert_eq!(f.values().len(), 2, "both sites' values are scrubbed");
        drop(f);
        // Only the embed's site has a jar: its jar alone.
        let only = for_urls(&repo, &["https://www.other.example/s", "https://x.com/a/status/1"], &work, false).unwrap();
        assert!(!std::fs::read_to_string(only.path()).unwrap().contains("news-value"));
        // The same site twice is one jar.
        drop(only);
        let twice = for_urls(&repo, &["https://x.com/a", "https://twitter.com/b"], &work, false).unwrap();
        assert_eq!(twice.values().len(), 1);
    }

    #[test]
    fn the_test_command_never_downloads_and_names_the_jar_by_path_only() {
        let args = test_args(Path::new("C:/t/cookies.txt"), "https://x.com/a/status/1");
        let joined = args.join(" ");
        assert!(joined.contains("--cookies C:/t/cookies.txt"));
        assert!(args.contains(&"--skip-download".to_string()) && args.contains(&"--dump-json".to_string()));
        assert_eq!(args.last().unwrap(), "https://x.com/a/status/1");
    }

    #[tokio::test]
    async fn a_test_without_a_jar_or_with_a_foreign_link_does_not_run_anything() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let nowhere = dir.path().join("no-such-yt-dlp.exe");
        let e = test_jar(&nowhere, dir.path(), &repo, "x.com", "https://x.com/a/status/1").await.unwrap_err();
        assert!(e.contains("no cookie jar"), "{e}");
        let jar = omni_core::cookies::parse_netscape("x.com", TEXT).unwrap();
        repo.put_cookie_jar("x.com", &jar, "admin", chrono::Utc::now()).unwrap();
        assert!(test_jar(&nowhere, dir.path(), &repo, "x.com", "https://example.org/a").await.is_err());
        // A missing yt-dlp is a failed test, stored, not a panic, and nothing is left behind.
        let t = test_jar(&nowhere, dir.path(), &repo, "x.com", "https://x.com/a/status/1?s=20").await.unwrap();
        assert!(!t.ok && !t.detail.contains("file-value-123456"));
        let stored = repo.list_cookie_jars().unwrap();
        assert_eq!(stored[0].last_test_ok, Some(false));
        assert_eq!(std::fs::read_dir(dir.path().join("cookies")).unwrap().count(), 0, "the test left a jar behind");
    }
}
