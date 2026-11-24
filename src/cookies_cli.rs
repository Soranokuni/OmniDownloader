//! `omni-ingest cookies sign-in|list` (plan P3.17): capture a site's login from a
//! dedicated sign-in browser profile into the sealed cookie jar.
//!
//! Run by the operator on the server's desktop. Prints counts and cookie names,
//! never a cookie value and never a link with its query string.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use omni_browser::signin::{self, BrowserKind, SigninEnd};
use omni_core::config::AppConfig;
use omni_core::cookies;
use omni_core::dependencies::DependencyManager;
use omni_core::paths::AppPaths;
use omni_core::repository::Repository;

#[derive(Subcommand)]
pub enum CookiesAction {
    /// Sign in to a site in a separate browser window, then store its login
    /// cookies. Run at the server's desktop in a prompt opened "as
    /// Administrator" (Εκτέλεση ως διαχειριστής). Domains: youtube.com, x.com,
    /// facebook.com, instagram.com, or another site's registrable domain.
    SignIn {
        /// The site, e.g. youtube.com. Without one, a numbered list to choose
        /// from (what the Start-menu entry «OmniIngest – Σύνδεση σε ιστότοπο» runs).
        domain: Option<String>,
        /// The browser to use: edge (default) or chrome
        #[arg(long)]
        browser: Option<String>,
    },
    /// List the stored cookie jars (no contents)
    List,
}

/// The plain-words error for a folder or database the signed-in user may not write to.
pub fn explain_access(e: anyhow::Error) -> anyhow::Error {
    let text = format!("{e:#}").to_lowercase();
    let denied = ["access is denied", "permission denied", "unable to open database", "readonly database", "attempt to write a readonly"];
    if denied.iter().any(|d| text.contains(d)) {
        // The cause stays under the hint: a missing drive or a blocked browser
        // also says "denied", and an elevated operator needs to see which.
        e.context("Access denied: run this from an Administrator prompt (Εκτέλεση ως διαχειριστής).")
    } else {
        e
    }
}

pub async fn run(paths: &AppPaths, action: CookiesAction) -> Result<()> {
    // From the Start menu (P13.9) the window closes with the process: whatever
    // happens, from reading the settings on, it waits for Enter (P13.9b).
    run_with(paths, action, &mut std::io::stdin().lock(), &mut std::io::stdout()).await
}

/// [`run`] with the window's input and output given (tests).
pub async fn run_with(paths: &AppPaths, action: CookiesAction, input: &mut dyn std::io::BufRead, screen: &mut dyn std::io::Write) -> Result<()> {
    let windowed = matches!(action, CookiesAction::SignIn { domain: None, .. });
    // The site list reads the same input: stdin is locked once, in `run` (P13.9c).
    let out = run_inner(paths, action, input, screen).await.map_err(explain_access);
    if windowed {
        hold_window(&out, input, screen);
    }
    out
}

/// Show the error, if any, and wait for Enter before the window closes.
pub fn hold_window(out: &Result<()>, input: &mut dyn std::io::BufRead, screen: &mut dyn std::io::Write) {
    if let Err(e) = out {
        let _ = writeln!(screen, "\n{e:#}");
    }
    let _ = writeln!(screen, "\nPress Enter to close this window. / Πατήστε Enter για να κλείσει το παράθυρο.");
    let _ = screen.flush();
    let mut line = String::new();
    let _ = input.read_line(&mut line);
}

async fn run_inner(paths: &AppPaths, action: CookiesAction, input: &mut dyn std::io::BufRead, screen: &mut dyn std::io::Write) -> Result<()> {
    let config = AppConfig::load_from_file(&paths.config).with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
    let repo = Repository::new(paths.resolve(&config.database_path))?.with_second_chance_window(&config.retry);
    match action {
        CookiesAction::List => list(&repo),
        CookiesAction::SignIn { domain: Some(domain), browser } => sign_in(paths, &config, &repo, &domain, browser.as_deref()).await,
        // From the Start menu (P13.9): choose a site; `run` keeps the window open.
        CookiesAction::SignIn { domain: None, browser } => {
            let picked = pick_site(input, screen)?;
            match picked {
                Some(domain) => sign_in(paths, &config, &repo, &domain, browser.as_deref()).await,
                None => {
                    writeln!(screen, "Nothing was changed. / Δεν άλλαξε τίποτα.")?;
                    Ok(())
                }
            }
        }
    }
}

/// The sites the list offers, in order.
pub const PICK_SITES: [&str; 4] = ["youtube.com", "x.com", "facebook.com", "instagram.com"];

/// Ask which site to sign in to (P13.9): a number from the list, another
/// site's domain typed in full, or nothing to stop. `None`: the operator
/// stopped. Asks again after an answer it cannot use, at most five times.
pub fn pick_site(input: &mut dyn std::io::BufRead, out: &mut dyn std::io::Write) -> Result<Option<String>> {
    writeln!(out, "Σε ποιον ιστότοπο να συνδεθεί ο λογαριασμός του σταθμού; / Which site do you want to sign in to?")?;
    for (i, site) in PICK_SITES.iter().enumerate() {
        writeln!(out, "  {}. {site}", i + 1)?;
    }
    writeln!(out, "  Or type another site's domain (example.com). Enter alone: stop. / Ή γράψτε άλλον ιστότοπο. Μόνο Enter: έξοδος.")?;
    for _ in 0..5 {
        write!(out, "> ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let answer = line.trim();
        if answer.is_empty() {
            return Ok(None);
        }
        if let Ok(n) = answer.parse::<usize>() {
            if (1..=PICK_SITES.len()).contains(&n) {
                return Ok(Some(PICK_SITES[n - 1].to_string()));
            }
        } else {
            // The site's registrable domain, as the jars are keyed (news.example.com → example.com).
            let host = answer.to_ascii_lowercase();
            let domain = (!host.contains(['/', ' ', ':']))
                .then(|| cookies::jar_domain_for_url(&format!("https://{host}/")))
                .flatten()
                .filter(|d| cookies::is_registrable_domain(d));
            if let Some(domain) = domain {
                return Ok(Some(domain));
            }
        }
        writeln!(out, "\"{}\" is not on the list or a site domain; type 1 to {} or a domain such as example.com.", answer.chars().take(60).collect::<String>(), PICK_SITES.len())?;
    }
    bail!("No site was chosen.")
}

fn list(repo: &Repository) -> Result<()> {
    let jars = repo.list_cookie_jars()?;
    if jars.is_empty() {
        println!("No cookie jars stored.");
    }
    for j in jars {
        let when = |t: Option<chrono::DateTime<chrono::Utc>>| t.map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default();
        let test = match j.last_test_ok {
            Some(true) => "test ok",
            Some(false) => "test FAILED",
            None => "not tested",
        };
        println!("{:<20} {:>3} cookies  updated {} by {}  expires {}  {}", j.domain, j.cookie_count, when(j.updated_at), j.updated_by, when(j.expires_min), test);
    }
    Ok(())
}

async fn sign_in(paths: &AppPaths, config: &AppConfig, repo: &Repository, raw_domain: &str, browser: Option<&str>) -> Result<()> {
    let domain = cookies::canonical_jar_domain(raw_domain.trim().trim_start_matches("www."));
    if !cookies::is_registrable_domain(&domain) {
        bail!("{raw_domain:?} is not a site domain such as youtube.com");
    }
    let preferred = match browser {
        Some(b) => Some(BrowserKind::parse(b).with_context(|| format!("--browser must be edge or chrome, not {b:?}"))?),
        None => None,
    };
    let Some((kind, exe)) = signin::find_browser(preferred) else {
        bail!("No Edge or Chrome found on this PC");
    };
    let profile = signin::profile_dir(&paths.data, kind);

    println!("Sign in to {domain} in the window that opens, then close the browser window.");
    println!("Συνδεθείτε στο {domain} στο παράθυρο που ανοίγει και μετά κλείστε το παράθυρο του browser.");
    let timed_out = match signin::run_signin_window(&exe, &profile, &signin::login_url(&domain)).await? {
        SigninEnd::Closed => false,
        SigninEnd::AlreadyOpen => bail!("The sign-in browser profile is already open in another window; close that window and run this again."),
        SigninEnd::TimedOut => {
            println!("The window was still open after 30 minutes and was closed; checking whether you signed in anyway…");
            true
        }
    };

    println!("Reading the login from the sign-in profile…");
    let who = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string());
    let stored = match signin::export_cookies(&exe, &profile, &domain).await {
        Ok(all) => signin::store_jar(repo, &domain, &all, &format!("sign-in ({who})")).map(|jar| (jar, all)),
        Err(e) => Err(e),
    };
    let (jar, all) = match stored {
        Ok(v) => v,
        // Only "no login cookies" means nobody signed in; any other failure
        // (jar limit, DB, launch) is shown as it is.
        Err(e) if timed_out && format!("{e:#}").contains("Not signed in") => {
            bail!("No sign-in within 30 minutes; the browser was closed. Nothing was saved.")
        }
        Err(e) => return Err(e),
    };
    println!("{}", signin::summary(&domain, &jar, &all));
    // A second chance for the site's jobs waiting in review for a login (P11.5).
    let requeued = omni_core::second_chance::after_cookies(repo, &domain);
    if requeued > 0 {
        println!("{requeued} job(s) waiting in review for {domain} were queued again.");
    }

    let ytdl = DependencyManager::new(&paths.bin).ytdl_live();
    let stored = repo.list_cookie_jars()?.into_iter().find(|j| j.domain == domain).and_then(|j| j.last_sample_url);
    match (ytdl.exists(), cookies::sample_url_for(&domain, stored.as_deref())) {
        (true, Some(url)) => {
            println!("Testing the login with yt-dlp on {}…", cookies::host_and_path(&url));
            let pacer = omni_broadcast::pacing::HostPacer::default();
            let ffmpeg = crate::resolve_ffmpeg(config, &DependencyManager::new(paths.resolve(&config.bin_dir)));
            match omni_broadcast::cookie_file::test_jar(&ytdl, Some(&ffmpeg), &paths.resolve(&config.temp_path), repo, &domain, &url, &pacer).await {
                Ok(t) if t.ok => println!("Test ok: yt-dlp reads a video with this login."),
                Ok(t) if t.inconclusive => println!("Not tested: {}.", t.detail),
                Ok(t) => println!("Test FAILED: {}", t.detail),
                Err(why) => println!("The test could not run: {why}"),
            }
        }
        _ => println!("The login was not tested (yt-dlp or a sample link is missing); Administration → Cookies can test it."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(answers: &str) -> (Result<Option<String>>, String) {
        let mut out = Vec::new();
        let got = pick_site(&mut answers.as_bytes(), &mut out);
        (got, String::from_utf8(out).unwrap())
    }

    /// P13.9c: with valid settings the site list reads the window's own
    /// input (stdin is locked once, by `run`): the list is shown on the
    /// window's screen, Enter alone stops, Enter again closes. Under a timeout,
    /// so a second lock of the same input fails the test instead of hanging it.
    #[tokio::test]
    async fn the_site_list_reads_the_same_input_as_the_window() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = AppPaths::with_root(dir.path(), "config.json");
        AppConfig::default().save_to_file(&paths.config).unwrap();
        let mut screen = Vec::new();
        let mut input = "\n\n".as_bytes();
        let action = CookiesAction::SignIn { domain: None, browser: None };
        let out = tokio::time::timeout(std::time::Duration::from_secs(20), run_with(&paths, action, &mut input, &mut screen))
            .await
            .expect("the sign-in did not finish: the input is read twice");
        assert!(out.is_ok(), "{out:?}");
        let shown = String::from_utf8(screen).unwrap();
        assert!(shown.contains("1. youtube.com"), "the list is not on the window's screen: {shown}");
        assert!(shown.contains("Nothing was changed") && shown.contains("Press Enter"), "{shown}");
        assert!(input.is_empty(), "both Enters were read from the window's input");
    }

    /// P13.9b: from the Start menu, an error before the site list (settings
    /// that cannot be read) is shown and the window waits for Enter.
    #[tokio::test]
    async fn an_error_before_the_list_keeps_the_window_open_with_it() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("config.json"), "{ not json").unwrap();
        let paths = AppPaths::with_root(dir.path(), "config.json");
        let mut screen = Vec::new();
        let out = run_with(&paths, CookiesAction::SignIn { domain: None, browser: None }, &mut "\n".as_bytes(), &mut screen).await;
        assert!(out.is_err());
        let shown = String::from_utf8(screen).unwrap();
        assert!(shown.contains("not valid JSON") && shown.contains("Press Enter"), "{shown}");

        // With a site named (a prompt the operator opened), nothing waits.
        let mut screen = Vec::new();
        let named = CookiesAction::SignIn { domain: Some("x.com".into()), browser: None };
        assert!(run_with(&paths, named, &mut "".as_bytes(), &mut screen).await.is_err());
        assert!(screen.is_empty());
    }

    /// P13.9: the Start-menu entry's site list.
    #[test]
    fn the_site_list_takes_a_number_or_a_domain_and_stops_on_enter() {
        let (got, shown) = pick("2\n");
        assert_eq!(got.unwrap().as_deref(), Some("x.com"));
        assert!(shown.contains("1. youtube.com") && shown.contains("4. instagram.com"), "{shown}");
        assert_eq!(pick("www.News.Example.com\n").0.unwrap().as_deref(), Some("example.com"));
        assert_eq!(pick("\n").0.unwrap(), None, "Enter alone stops");
        assert_eq!(pick("").0.unwrap(), None, "a closed input stops");
        let (got, shown) = pick("9\nnot a site\n1\n");
        assert_eq!(got.unwrap().as_deref(), Some("youtube.com"), "asked again after two bad answers");
        assert_eq!(shown.matches("is not on the list").count(), 2, "{shown}");
        assert!(pick("9\n9\n9\n9\n9\n").0.is_err(), "five bad answers end it");
    }

    #[test]
    fn a_denied_folder_or_database_says_to_use_an_administrator_prompt() {
        for raw in ["Access is denied. (os error 5)", "unable to open database file", "attempt to write a readonly database"] {
            let e = anyhow::anyhow!(raw.to_string()).context("Failed creating SQLite connection pool");
            let err = explain_access(e);
            assert!(err.to_string().contains("Administrator prompt"), "{err:#}");
            // The cause is kept under the hint for an operator who is already elevated.
            assert!(format!("{err:#}").contains(raw), "{err:#}");
        }
        assert_eq!(explain_access(anyhow::anyhow!("something else")).to_string(), "something else");
    }
}
