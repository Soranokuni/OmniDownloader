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
        /// The site, e.g. youtube.com
        domain: String,
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
    run_inner(paths, action).await.map_err(explain_access)
}

async fn run_inner(paths: &AppPaths, action: CookiesAction) -> Result<()> {
    let config = AppConfig::load_from_file(&paths.config).with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
    let repo = Repository::new(paths.resolve(&config.database_path))?;
    match action {
        CookiesAction::List => list(&repo),
        CookiesAction::SignIn { domain, browser } => sign_in(paths, &config, &repo, &domain, browser.as_deref()).await,
    }
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

    let ytdl = DependencyManager::new(&paths.bin).ytdl_live();
    let stored = repo.list_cookie_jars()?.into_iter().find(|j| j.domain == domain).and_then(|j| j.last_sample_url);
    match (ytdl.exists(), cookies::sample_url_for(&domain, stored.as_deref())) {
        (true, Some(url)) => {
            println!("Testing the login with yt-dlp on {}…", cookies::host_and_path(&url));
            match omni_broadcast::cookie_file::test_jar(&ytdl, &paths.resolve(&config.temp_path), repo, &domain, &url).await {
                Ok(t) if t.ok => println!("Test ok: yt-dlp reads a video with this login."),
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
