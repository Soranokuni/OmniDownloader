//! Sign-in browser profile and cookie export (plan P3.17).
//!
//! YouTube answers "Sign in to confirm you're not a bot" to a server. Chrome
//! and Edge seal the cookies of the user's own profile to the app, and Google
//! refuses a sign-in in a browser that was started for automation, so the
//! login is captured in two phases, both run by the operator at the server's
//! desktop: (1) a plain browser window on a profile of our own, where a person
//! signs in; (2) the same profile opened headless, to read its cookies over
//! CDP into the sealed cookie jar.
//!
//! A cookie jar is a login: nothing here prints, logs or returns a cookie
//! value in text meant for people. The summary names cookies and counts them.

use anyhow::{bail, Context, Result};
use omni_core::cookies::{self, ParsedJar};
use omni_core::repository::Repository;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::info;

use crate::browser::HeadlessBrowserManager;

/// How long the operator has to sign in before the window is closed for them.
pub const SIGNIN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A browser that exits sooner than this handed its window to a process that already had the profile.
pub const HANDOFF_WITHIN: Duration = Duration::from_secs(2);
/// Time given to the page's own scripts to set their cookies after load.
const SETTLE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserKind {
    Edge,
    Chrome,
}

impl BrowserKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "edge" => Some(Self::Edge),
            "chrome" => Some(Self::Chrome),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Chrome => "chrome",
        }
    }

    fn candidates(self) -> &'static [&'static str] {
        match self {
            Self::Edge => &[
                r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
                r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            ],
            Self::Chrome => &[
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
                r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            ],
        }
    }
}

/// The browser to use: the one asked for, else Edge, then Chrome.
pub fn find_browser(preferred: Option<BrowserKind>) -> Option<(BrowserKind, PathBuf)> {
    let order = match preferred {
        Some(k) => vec![k],
        None => vec![BrowserKind::Edge, BrowserKind::Chrome],
    };
    order.into_iter().find_map(|k| k.candidates().iter().map(Path::new).find(|p| p.exists()).map(|p| (k, p.to_path_buf())))
}

/// The persistent profile of the sign-in browser: `<data>/signin-profile/<browser>`.
pub fn profile_dir(data: &Path, kind: BrowserKind) -> PathBuf {
    data.join("signin-profile").join(kind.name())
}

/// Where a person signs in to `domain` (canonical jar domain).
pub fn login_url(domain: &str) -> String {
    match domain {
        "youtube.com" => "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F".to_string(),
        "x.com" => "https://x.com/i/flow/login".to_string(),
        "facebook.com" => "https://www.facebook.com/login".to_string(),
        "instagram.com" => "https://www.instagram.com/accounts/login/".to_string(),
        other => format!("https://www.{other}/"),
    }
}

/// The command line of phase 1: a plain browser window. No debugging port, no
/// headless mode, no automation switch: Google refuses a sign-in without them.
pub fn signin_launch_args(profile: &Path, url: &str) -> Vec<String> {
    vec![
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        // Without it Edge keeps running in the background after its window closes.
        "--disable-background-mode".to_string(),
        url.to_string(),
    ]
}

/// Whether another browser process holds the profile (Chrome keeps `lockfile`
/// open, exclusively on Windows, while it runs).
pub fn profile_in_use(profile: &Path) -> bool {
    let lock = profile.join("lockfile");
    lock.exists() && std::fs::OpenOptions::new().write(true).open(&lock).is_err()
}

#[derive(Debug, PartialEq, Eq)]
pub enum SigninEnd {
    /// The window was closed by the operator.
    Closed,
    /// The browser left at once: the profile is open in another window.
    AlreadyOpen,
    /// 30 minutes passed; the browser's process tree was killed.
    TimedOut,
}

/// Phase 1: open the browser on the sign-in profile and wait for it to close.
pub async fn run_signin_window(exe: &Path, profile: &Path, url: &str) -> Result<SigninEnd> {
    std::fs::create_dir_all(profile).with_context(|| format!("Could not create the sign-in profile {profile:?}"))?;
    if profile_in_use(profile) {
        return Ok(SigninEnd::AlreadyOpen);
    }
    let out = omni_core::process::run(exe, signin_launch_args(profile, url), omni_core::process::RunOpts::new(SIGNIN_TIMEOUT))
        .await
        .context("Could not start the browser")?;
    Ok(if out.timed_out {
        SigninEnd::TimedOut
    } else if out.elapsed < HANDOFF_WITHIN {
        SigninEnd::AlreadyOpen
    } else {
        SigninEnd::Closed
    })
}

/// One cookie as the browser reports it. Not `Debug`: it holds a login.
#[derive(Clone)]
pub struct RawCookie {
    pub domain: String,
    pub path: String,
    pub secure: bool,
    /// Seconds since the epoch; `None` for a session cookie.
    pub expires: Option<i64>,
    pub name: String,
    pub value: String,
}

impl RawCookie {
    pub fn from_cdp(c: &chromiumoxide::cdp::browser_protocol::network::Cookie) -> Self {
        Self {
            domain: c.domain.clone(),
            path: c.path.clone(),
            secure: c.secure,
            expires: (!c.session && c.expires > 0.0).then(|| c.expires as i64),
            name: c.name.clone(),
            value: c.value.clone(),
        }
    }
}

/// Whether a cookie's domain belongs to the jar for `jar_domain`: the domain
/// itself or a subdomain of it (or of a name the jar also covers).
pub fn belongs_to_jar(cookie_domain: &str, jar_domain: &str) -> bool {
    let host = cookie_domain.trim().trim_start_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    let covered = cookies::jar_covers(jar_domain);
    let ours: Vec<&str> = if covered.is_empty() { vec![jar_domain] } else { covered.to_vec() };
    ours.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

fn clean(s: &str) -> bool {
    !s.chars().any(|c| c.is_control())
}

/// Netscape `cookies.txt` for the cookies of `jar_domain`, and how many cookies
/// were left out because a field could not be written safely (a tab, newline
/// or other control character would corrupt a line).
pub fn to_netscape(jar_domain: &str, all: &[RawCookie]) -> (String, usize) {
    let mut text = String::from("# Netscape HTTP Cookie File\n");
    let mut skipped = 0usize;
    for c in all.iter().filter(|c| belongs_to_jar(&c.domain, jar_domain)) {
        let path = if c.path.is_empty() { "/" } else { c.path.as_str() };
        if c.name.is_empty() || !path.starts_with('/') || ![&c.domain, &c.path, &c.name, &c.value].iter().all(|f| clean(f)) {
            skipped += 1;
            continue;
        }
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            c.domain,
            if c.domain.starts_with('.') { "TRUE" } else { "FALSE" },
            path,
            if c.secure { "TRUE" } else { "FALSE" },
            c.expires.unwrap_or(0).max(0),
            c.name,
            c.value
        ));
    }
    (text, skipped)
}

/// A validated jar from exported cookies, or the "not signed in" error.
/// `Err` means nothing may be stored.
pub fn build_jar(jar_domain: &str, all: &[RawCookie]) -> Result<ParsedJar> {
    let not_signed_in = || anyhow::anyhow!("Not signed in to {jar_domain}: no login cookies found; nothing was saved");
    let (text, _skipped) = to_netscape(jar_domain, all);
    let login = cookies::login_cookie_names(jar_domain);
    let has_login = all
        .iter()
        .filter(|c| belongs_to_jar(&c.domain, jar_domain))
        .any(|c| login.is_empty() || login.contains(&c.name.as_str()));
    if !has_login {
        return Err(not_signed_in());
    }
    cookies::parse_netscape(jar_domain, &text).map_err(|_| not_signed_in())
}

/// Build the jar and store it. Nothing is stored when it is not a login.
pub fn store_jar(repo: &Repository, jar_domain: &str, all: &[RawCookie], updated_by: &str) -> Result<ParsedJar> {
    let jar = build_jar(jar_domain, all)?;
    repo.put_cookie_jar(jar_domain, &jar, updated_by, chrono::Utc::now()).context("The cookie jar could not be stored")?;
    Ok(jar)
}

/// What to print about a stored jar: counts and cookie names, never a value.
pub fn summary(jar_domain: &str, jar: &ParsedJar, all: &[RawCookie]) -> String {
    let login = cookies::login_cookie_names(jar_domain);
    let mut names: Vec<&str> = all
        .iter()
        .filter(|c| belongs_to_jar(&c.domain, jar_domain) && login.contains(&c.name.as_str()))
        .map(|c| c.name.as_str())
        .collect();
    names.sort();
    names.dedup();
    let expiry = match jar.expires_min {
        Some(t) => format!("login expires {}", t.format("%Y-%m-%d")),
        None => "login lasts until the browser session ends".to_string(),
    };
    let named = if names.is_empty() { "none named".to_string() } else { names.join(", ") };
    format!("Saved {} cookies for {jar_domain} (login cookies: {named}); {expiry}.", jar.count)
}

/// Phase 2: open the profile headless with the debugging port, load the site,
/// and return the cookies of the jar's site (HttpOnly ones included).
pub async fn export_cookies(exe: &Path, profile: &Path, jar_domain: &str) -> Result<Vec<RawCookie>> {
    let mut session = HeadlessBrowserManager::launch_persistent(exe, profile)
        .await
        .context("Could not open the sign-in profile (is its window still open?)")?;
    let work = async {
        let url = format!("https://www.{jar_domain}/");
        let page = session.browser.new_page(url.as_str()).await.context("Could not open the page")?;
        let _ = tokio::time::timeout(Duration::from_secs(45), page.wait_for_navigation()).await;
        tokio::time::sleep(SETTLE).await;
        let cdp = session.browser.get_cookies().await.context("Could not read the cookies")?;
        info!("Sign-in export for {jar_domain}: {} cookies in the profile", cdp.len());
        Ok::<_, anyhow::Error>(cdp.iter().map(RawCookie::from_cdp).collect::<Vec<_>>())
    }
    .await;
    session.shutdown().await;
    let all = work?;
    if all.is_empty() {
        bail!("The sign-in profile has no cookies");
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(domain: &str, name: &str, value: &str) -> RawCookie {
        RawCookie { domain: domain.into(), path: "/".into(), secure: true, expires: Some(1_900_000_000), name: name.into(), value: value.into() }
    }

    #[test]
    fn the_sign_in_window_is_a_plain_browser() {
        let args = signin_launch_args(Path::new(r"C:\omni\data\signin-profile\edge"), &login_url("youtube.com"));
        for a in &args {
            for banned in ["--remote-debugging", "--headless", "--enable-automation"] {
                assert!(!a.contains(banned), "{a}");
            }
        }
        assert!(args.iter().any(|a| a.starts_with("--user-data-dir=") && a.ends_with("signin-profile\\edge")));
        assert!(args.contains(&"--disable-background-mode".to_string()));
        assert!(args.contains(&"--no-first-run".to_string()) && args.contains(&"--no-default-browser-check".to_string()));
        assert!(args.last().unwrap().starts_with("https://accounts.google.com/ServiceLogin?service=youtube"));
    }

    #[test]
    fn login_urls_per_site() {
        assert_eq!(login_url("x.com"), "https://x.com/i/flow/login");
        assert_eq!(login_url("facebook.com"), "https://www.facebook.com/login");
        assert_eq!(login_url("instagram.com"), "https://www.instagram.com/accounts/login/");
        assert_eq!(login_url("news.example"), "https://www.news.example/");
    }

    #[test]
    fn the_domain_filter_keeps_the_sites_cookies_only() {
        for d in [".youtube.com", "www.youtube.com", "youtube.com", ".youtu.be"] {
            assert!(belongs_to_jar(d, "youtube.com"), "{d}");
        }
        for d in [".google.com", "notyoutube.com", "youtube.com.evil.net", ".accounts.google.com", ""] {
            assert!(!belongs_to_jar(d, "youtube.com"), "{d}");
        }
        assert!(belongs_to_jar(".twitter.com", "x.com") && belongs_to_jar(".x.com", "x.com"));
        assert!(!belongs_to_jar(".facebook.com", "x.com"));
    }

    #[test]
    fn netscape_flags_and_round_trip() {
        let mut session = c(".youtube.com", "PREF", "pv");
        session.expires = None;
        let mut insecure = c("www.youtube.com", "host_only", "hv");
        insecure.secure = false;
        let all = vec![c(".youtube.com", "SID", "sid-value-123456"), insecure, session, c(".google.com", "NID", "other-value-1")];
        let (text, skipped) = to_netscape("youtube.com", &all);
        assert_eq!(skipped, 0);
        assert!(text.starts_with("# Netscape HTTP Cookie File\n"));
        assert!(text.contains(".youtube.com\tTRUE\t/\tTRUE\t1900000000\tSID\tsid-value-123456\n"));
        assert!(text.contains("www.youtube.com\tFALSE\t/\tFALSE\t1900000000\thost_only\thv\n"));
        assert!(text.contains(".youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tpv\n"), "a session cookie has expiry 0");
        assert!(!text.contains("NID"));
        let parsed = cookies::parse_netscape("youtube.com", &text).unwrap();
        assert_eq!((parsed.count, parsed.malformed, parsed.other_domain), (3, 0, 0));
    }

    #[test]
    fn a_value_that_would_corrupt_a_line_is_skipped() {
        let all = vec![c(".youtube.com", "SID", "good-value-123"), c(".youtube.com", "A", "tab\there"), c(".youtube.com", "B", "new\nline"), c(".youtube.com", "C", "cr\rx")];
        let (text, skipped) = to_netscape("youtube.com", &all);
        assert_eq!(skipped, 3);
        assert_eq!(text.lines().count(), 2);
        let parsed = cookies::parse_netscape("youtube.com", &text).unwrap();
        assert_eq!((parsed.count, parsed.malformed), (1, 0));
    }

    #[test]
    fn x_keeps_the_twitter_cookies() {
        let all = vec![c(".x.com", "auth_token", "tok-value-12345"), c(".twitter.com", "ct0", "ct0-value-12345"), c(".google.com", "NID", "n-value-12345")];
        let jar = build_jar("x.com", &all).unwrap();
        assert_eq!(jar.count, 2);
    }

    #[test]
    fn no_login_cookie_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        // Visitor cookies only: YouTube without a login.
        let anon = vec![c(".youtube.com", "VISITOR_INFO1_LIVE", "visitor-value-1"), c(".youtube.com", "YSC", "ysc-value-12345")];
        let err = store_jar(&repo, "youtube.com", &anon, "sign-in (test)").unwrap_err().to_string();
        assert_eq!(err, "Not signed in to youtube.com: no login cookies found; nothing was saved");
        assert!(!repo.has_cookie_jar("youtube.com").unwrap(), "nothing may be stored");
        // A login cookie on another site does not count.
        let other = vec![c(".google.com", "SID", "sid-value-123456")];
        assert!(store_jar(&repo, "youtube.com", &other, "sign-in (test)").is_err());
        assert!(!repo.has_cookie_jar("youtube.com").unwrap());
        // With a login cookie the jar is stored, and says who stored it.
        let ok = vec![c(".youtube.com", "SID", "sid-value-123456")];
        let jar = store_jar(&repo, "youtube.com", &ok, "sign-in (tester)").unwrap();
        assert_eq!(jar.count, 1);
        let info = repo.list_cookie_jars().unwrap();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].updated_by, "sign-in (tester)");
    }

    #[test]
    fn the_summary_never_shows_a_value() {
        let all = vec![c(".youtube.com", "SID", "very-secret-value-1"), c(".youtube.com", "LOGIN_INFO", "very-secret-value-2"), c(".youtube.com", "YSC", "very-secret-value-3")];
        let jar = build_jar("youtube.com", &all).unwrap();
        let line = summary("youtube.com", &jar, &all);
        assert!(line.contains("3 cookies") && line.contains("LOGIN_INFO, SID"), "{line}");
        assert!(!line.contains("secret"), "{line}");
        assert!(!format!("{jar:?}").contains("secret"));
    }

    #[test]
    fn the_profile_lives_under_data() {
        assert_eq!(profile_dir(Path::new(r"C:\omni\data"), BrowserKind::Edge), Path::new(r"C:\omni\data").join("signin-profile").join("edge"));
        assert_eq!(BrowserKind::parse("Chrome"), Some(BrowserKind::Chrome));
        assert_eq!(BrowserKind::parse("firefox"), None);
    }
}
