use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Semaphore};
use tracing::{error, info, warn};

use omni_broadcast::pipeline::BroadcastEngine;
use omni_browser::sniffer::StreamSniffer;
use omni_core::config::AppConfig;
use omni_core::paths::AppPaths;
use omni_core::dependencies::DependencyManager;
use omni_core::models::{JobStage, JobStatus};
use omni_core::repository::Repository;
use omni_core::secrets::SecretStore;
use omni_core::health::HealthState;
use omni_core::logging::{LogGuard, Redactions};
use omni_core::update_gate::UpdateGate;
use omni_email::watcher::EmailWatcher;
use omni_web::server::WebServer;
use omni_web::state::AppState;

mod benchmark;
mod canary;
mod cookies_cli;
mod egress;
mod repair;
mod report_cli;
mod selfcheck;
mod site_memory;
mod worker;

use selfcheck::UpdateVerdict;
#[cfg(debug_assertions)]
use worker::finish_held;
use worker::{run_job, wait_for_deliveries, LEASE_REAP_INTERVAL, LEASE_SECS, SHUTDOWN_DELIVERY_BUDGET};

#[derive(Parser)]
#[command(name = "omni-ingest")]
#[command(author = "Alex Fountas")]
#[command(version = "1.0.0")]
#[command(about = "Unified Broadcast-Grade Media Ingest Engine & Windows Service")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long, default_value = "config.json", global = true)]
    config: String,

    /// Install directory to resolve relative paths against.
    ///
    /// Defaults to the directory holding omni-ingest.exe (or `$OMNI_ROOT`).
    /// Never the process working directory: under the Windows SCM that is
    /// `C:\Windows\System32` (plan P0.1, defect W-10).
    #[arg(long, global = true)]
    root: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the ingest daemon in console/interactive mode
    Run,
    /// Run as Windows Service entry point (invoked by Windows SCM)
    RunService,
    /// Run the interactive CLI setup wizard
    Setup,
    /// Install (or upgrade) from this release folder: copy to the install
    /// directory, register and start the Windows service, check it answers
    Install {
        /// Install directory
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        /// Dalet watchfolder (asked on a new installation when not given)
        #[arg(long)]
        watchfolder: Option<String>,
        /// Web panel port
        #[arg(long)]
        port: Option<u16>,
        /// Run the service as this account (a domain account for a network
        /// watchfolder); `NT SERVICE\OmniIngestService` when not given,
        /// `LocalSystem` to opt out. An upgrade keeps the installed account
        #[arg(long)]
        account: Option<String>,
        /// Ask nothing; take the defaults
        #[arg(long)]
        yes: bool,
        /// Leave the Windows firewall alone
        #[arg(long)]
        no_firewall: bool,
        /// Bring the configuration, database, secrets and seeds of an older
        /// installation (or a dev checkout) on this PC into the new one
        #[arg(long)]
        import: Option<std::path::PathBuf>,
    },
    /// Remove the Windows service and its firewall rule; files stay
    Uninstall {
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// Manage the Windows Service (install, uninstall, start, stop, status)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Check the browser and every self-check link now, and print the
    /// results (the same check that runs every morning; nothing is downloaded)
    Selfcheck,
    /// Run the stored 65-link benchmark, list its runs, or print a report
    Benchmark {
        #[command(subcommand)]
        action: benchmark::BenchAction,
    },
    /// Cookie jars for login-walled sites: sign in at this PC's desktop
    /// (YouTube, X, Facebook, Instagram), or list the stored jars
    Cookies {
        #[command(subcommand)]
        action: cookies_cli::CookiesAction,
    },
    /// Test headless browser stream sniffing on a target URL
    BrowserTest {
        /// URL to navigate and extract video stream from
        url: String,
    },
    /// Manage and update HaGeZi & Greek adblock filter lists
    Adblock {
        #[command(subcommand)]
        action: AdblockAction,
    },
    /// Show what the parser would do with recent mail, without queueing
    /// anything or touching the mailbox (read-only)
    MailPreview {
        /// How many of the most recently changed Inbox messages to show
        #[arg(long, default_value_t = 10)]
        last: usize,
        /// Look this many hours back
        #[arg(long, default_value_t = 72)]
        hours: i64,
        /// Parse these .eml files instead of reading the mailbox
        #[arg(long)]
        eml: Vec<std::path::PathBuf>,
        /// Also ask the LLM assist, as the daemon would
        #[arg(long)]
        llm: bool,
    },
    /// Queue saved .eml files as if they had arrived in the mailbox: parsed,
    /// queued, attachments saved, the mail kept for the MCR mail view. A file
    /// already handled is skipped. The running daemon downloads the jobs.
    MailIngest {
        /// The .eml files
        #[arg(long, required = true)]
        eml: Vec<std::path::PathBuf>,
    },
    /// Reports from the database the daemon writes (read only; safe while it runs)
    Report {
        #[command(subcommand)]
        what: report_cli::ReportKind,
    },
    /// The mail the daemon has handled, newest first
    MailHistory {
        #[arg(long, default_value_t = 20)]
        last: i64,
    },
    /// Have the running daemon read a handled mail again on its next poll;
    /// name it by Message-ID or by a part of its subject
    MailReprocess {
        #[arg(long)]
        message_id: Option<String>,
        #[arg(long)]
        subject: Option<String>,
    },
    /// Account recovery on this machine: list accounts, reset a password
    Admin {
        #[command(subcommand)]
        action: AdminAction,
    },
    /// Manage credentials in the encrypted secret store
    Secrets {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Development only (debug builds): make a real held video from a local
    /// file, so the MCR desk's «Για έγκριση» can be seen. Use a dev config.
    #[cfg(debug_assertions)]
    DevHold {
        /// The video (or audio) file to hold; it is copied, never moved
        #[arg(long)]
        file: std::path::PathBuf,
        /// File name without `.mxf` (default `1_{JOURNALIST}_DEVHOLD`)
        #[arg(long)]
        slug: Option<String>,
        /// Journalist surname for the job
        #[arg(long)]
        journalist: Option<String>,
        /// A picture to show as the article's image on the card, next to the
        /// closest frame of the preview (also records a held repair attempt)
        #[arg(long)]
        article_image: Option<std::path::PathBuf>,
        /// Record a held repair attempt with a fixed, invented checklist and two
        /// more verified candidates (the card then offers «Επόμενη επιλογή (2)»)
        #[arg(long)]
        checks: bool,
    },
    /// Development only (debug builds): add one invented overnight self-heal
    /// report, so the desk's morning line and the admin «Self-heal» card can be
    /// seen. Use a dev config.
    #[cfg(debug_assertions)]
    DevSelfhealReport {
        /// The site (default `news.dev-selfheal.example`)
        #[arg(long, default_value = "news.dev-selfheal.example")]
        domain: String,
        /// found | nothing | skipped | outage
        #[arg(long, default_value = "found")]
        outcome: String,
        /// shipped | admin | llm
        #[arg(long, default_value = "admin")]
        origin: String,
        /// Canary pages the recipe worked on
        #[arg(long, default_value_t = 2)]
        pages: usize,
        /// Hours ago the run happened
        #[arg(long, default_value_t = 1)]
        hours_ago: i64,
    },
    /// Debug builds only: invented sites, canaries, recipes and repair attempts,
    /// so the admin's Sites, Recipes and Repair-report cards can be seen. Use a
    /// dev config (refused on the install's own config and database).
    #[cfg(debug_assertions)]
    DevSeedRepair,
}

#[derive(Subcommand)]
enum AdminAction {
    /// List every account's sign-in address, role and whether it is active
    ListUsers,
    /// Set a new password for an account (prompted, never on the command
    /// line) and end all of its sessions
    ResetPassword { email: String },
    /// Sign an account out and refuse its sign-in until reactivated
    Deactivate { email: String },
    /// Let a deactivated account sign in again
    Activate { email: String },
}

#[derive(Subcommand)]
enum SecretAction {
    /// Show which secrets are set (values are never printed)
    List,
    /// Set a secret; the value is prompted for, never taken from the command
    /// line, so it cannot leak through shell history or the process list
    Set { key: String },
    /// Remove a secret
    Clear { key: String },
}

#[derive(Subcommand)]
enum AdblockAction {
    /// Update HaGeZi and Greek AdBlock filter lists from upstream repositories
    Update,
    /// Display current active rules and domain statistics
    Status,
}

#[derive(Subcommand)]
enum ServiceAction {
    Install {
        /// Account the service runs as, e.g. `DOMAIN\svc_omni` (the password is
        /// prompted for). Omit for the virtual account `NT SERVICE\OmniIngestService`;
        /// `LocalSystem` to opt out. Both authenticate to an SMB share as the
        /// computer account (defect W-13): the installer warns when the
        /// watchfolder is one.
        #[arg(long)]
        account: Option<String>,
    },
    Uninstall,
    Start,
    Stop,
    Status,
}

/// Set up logging for whichever subcommand is running.
///
/// The daemon writes rotated files; `run` additionally writes the console,
/// because a human is watching it. `run-service` does not, because the SCM
/// discards stdout — which is exactly how the previous build managed to throw
/// away every diagnostic it produced in the newsroom.
///
/// The short-lived subcommands get console-only logging: they should not open,
/// rotate or prune the file the running service owns.
fn init_logging(cli: &Cli, paths: &AppPaths) -> Result<Option<LogGuard>> {
    let daemon_mode = matches!(
        cli.command,
        None | Some(Commands::Run) | Some(Commands::RunService)
    );
    if !daemon_mode {
        omni_core::logging::init_console_only("info,chromiumoxide=off");
        return Ok(None);
    }

    // Read the log settings straight from the file: this runs before the
    // daemon's own config load, and a config that fails to parse should still
    // produce a log saying so.
    let config = AppConfig::load_from_file(&paths.config).unwrap_or_default();
    let console = !matches!(cli.command, Some(Commands::RunService));

    // Seed the redaction set from the secret store before the first line is
    // written, so nothing can be logged in the window before it is populated.
    let store = SecretStore::new(paths.resolve("data/secrets.bin"));
    let mut values = store.all_values();
    // A secret supplied through the environment (plan P4.9) is redacted too.
    values.extend(
        omni_core::config::env_vars::SECRETS
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
    );
    let redactions = Redactions::new(values);

    let guard = omni_core::logging::init(&paths.logs, &config.log, console, redactions)?;
    Ok(Some(guard))
}

/// `omni-ingest mail-preview` (plan P4.16): parse recent mail, or .eml
/// files, and print what would be queued. Reads the mailbox, the roster and
/// `processed_mail`; writes nothing, marks nothing.
async fn mail_preview(
    paths: &AppPaths,
    last: usize,
    hours: i64,
    eml: Vec<std::path::PathBuf>,
    llm: bool,
) -> Result<()> {
    use omni_email::source::MailSource;

    let mut config = AppConfig::load_from_file(&paths.config)
        .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
    config.adopt_secrets(&SecretStore::new(paths.resolve("data/secrets.bin")))?;
    config.apply_env_overrides(|name| std::env::var(name).ok());
    let repo = Repository::new(paths.resolve(&config.database_path))?;
    let roster = repo.list_journalists()?;
    let groups = repo.list_groups()?;
    if roster.is_empty() {
        println!("! The journalist roster is empty: every mail will resolve to MCR.");
    }

    let mails: Vec<omni_email::InboundMail> = if !eml.is_empty() {
        let mut v = Vec::new();
        for path in &eml {
            let raw = std::fs::read(path).with_context(|| format!("Cannot read {path:?}"))?;
            v.push(omni_email::InboundMail::from_rfc822(&path.display().to_string(), &raw)?);
        }
        v
    } else {
        let source = omni_email::graph::GraphMailSource::new(config.graph.clone());
        if !source.is_configured() {
            anyhow::bail!("The Graph mailbox is not configured (tenant id, client id, mailbox, client secret).");
        }
        let since = chrono::Utc::now() - chrono::Duration::hours(hours.max(1));
        let mut headers = source.list_changed(since, 5000).await?;
        headers.sort_by_key(|h| std::cmp::Reverse(h.modified_at));
        headers.truncate(last.max(1));
        println!(
            "{} message(s) changed in the last {hours} h in {}; showing the newest {}.",
            headers.len(),
            config.graph.mailbox,
            headers.len()
        );
        let mut v = Vec::new();
        for h in headers {
            match source.fetch_mail(&h.id).await {
                Ok(m) => v.push(m),
                Err(e) => println!("! '{}': could not be read: {e:#}", h.subject),
            }
        }
        v
    };

    let assist = llm.then(|| {
        omni_email::assist::Assist::new(&config.ollama_endpoint, &config.ollama_model, config.llm.clone())
    });
    for mail in &mails {
        let parsed = omni_email::assist::interpret(mail, &roster, &groups, &config.parser, assist.as_ref()).await;
        let seen = repo
            .get_processed_mail(&omni_email::watcher::mail_key(mail))?
            .map(|p| p.outcome);
        print!("{}", omni_email::preview::render(mail, &parsed, seen.as_deref()));
    }
    println!("
Nothing was queued and the mailbox was not changed.");
    Ok(())
}

/// `omni-ingest mail-ingest --eml …` (plan P7.9): saved mail through the
/// watcher's own path into the configured database. Reads no mailbox.
/// `dev-hold` makes jobs and files: it must never touch the live installation.
/// Refused unless `--config` was given and neither the config nor the database
/// is the install root's own.
#[cfg(debug_assertions)]
fn dev_hold_guard(paths: &AppPaths, config_given: bool, db: &std::path::Path) -> Result<()> {
    let same = |a: &std::path::Path, b: &std::path::Path| {
        let key = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
        key(a) == key(b)
    };
    anyhow::ensure!(config_given, "dev-hold needs an explicit --config pointing at a throwaway dev config");
    anyhow::ensure!(
        !same(&paths.config, &paths.root.join("config.json")),
        "dev-hold refuses the install's own config.json; use a dev config"
    );
    anyhow::ensure!(
        !same(db, &paths.root.join("data").join("omni.db")),
        "dev-hold refuses the install's own database (data/omni.db); use a dev config"
    );
    Ok(())
}

/// Debug builds only: a real held video from a local file, made the way the
/// worker makes one (copy as an attachment source, lease, transcode, gate,
/// hold, preview, park). Stop the daemon on that config first, so no worker
/// leases the job (or another one) before this does.
#[cfg(debug_assertions)]
async fn dev_hold(
    paths: &AppPaths,
    file: &std::path::Path,
    slug: Option<String>,
    journalist: Option<String>,
    article_image: Option<std::path::PathBuf>,
    checks: bool,
) -> Result<()> {
    use omni_broadcast::pipeline::{DeliveryMode, RunOutcome};

    anyhow::ensure!(file.is_file(), "{file:?} is not a file");
    let config_given = std::env::args().any(|a| a == "-c" || a == "--config" || a.starts_with("--config="));
    let config = AppConfig::load_from_file(&paths.config)
        .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
    let db_path = paths.resolve(&config.database_path);
    dev_hold_guard(paths, config_given, &db_path)?;
    let repo = Repository::new(&db_path)?;
    let temp_path = paths.resolve(&config.temp_path);
    let watchfolder_path = paths.resolve(&config.watchfolder_path);
    let deps = DependencyManager::new(&paths.resolve(&config.bin_dir));
    let tool = |configured: &Option<String>, name: &str| {
        configured
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| deps.find_binary(name))
            .unwrap_or_else(|| deps.unresolved_path(name))
    };
    let ffmpeg = tool(&config.ffmpeg_path, "ffmpeg");
    let engine = BroadcastEngine::new(
        repo.clone(),
        tool(&config.ytdl_path, "yt-dlp"),
        ffmpeg.clone(),
        tool(&config.ffprobe_path, "ffprobe"),
        tool(&config.bmxtranswrap_path, "bmxtranswrap"),
        temp_path.clone(),
        watchfolder_path,
    );
    if let Some(img) = &article_image {
        anyhow::ensure!(img.is_file(), "{img:?} is not a file");
    }

    let journalist = journalist.unwrap_or_else(|| "DEVTEST".to_string()).to_uppercase();
    let slug = slug.unwrap_or_else(|| format!("1_{journalist}_DEVHOLD"));
    // Unique, so the dedup window never folds two runs into one job.
    let nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let mut new = omni_core::models::NewJob::new(format!("attachment://dev-hold/{nonce}"), slug, journalist);
    new.keyword = "DEVHOLD".into();
    new.status = JobStatus::ManualDownload;
    new.extraction_method = Some("attachment".into());
    let id = repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();

    let dir = temp_path.join("attachments").join(id.to_string());
    std::fs::create_dir_all(&dir)?;
    let source = dir.join(file.file_name().context("the file has no name")?);
    std::fs::copy(file, &source).with_context(|| format!("Could not copy {file:?} to {source:?}"))?;
    anyhow::ensure!(repo.attach_source(id, &source.to_string_lossy())?, "job #{id} could not take its source");

    let owner = format!("{}:dev-hold", hostname_for_lease());
    // Exactly our job, never the next one in the queue. A daemon on this dev
    // database could take it first; then it is cancelled, not left to run.
    if !repo.lease_job_by_id(id, &owner, 3600)? {
        let _ = repo.cancel_job(id);
        anyhow::bail!("job #{id} could not be leased (a daemon on this dev database took it?); it was cancelled");
    }
    let job = repo.get_job(id)?.context("the job vanished")?;
    let mode = DeliveryMode::Hold {
        hold_hours: config.repair.hold_hours,
        held_max_bytes: config.repair.held_max_gb.saturating_mul(1024 * 1024 * 1024),
    };
    match engine.process_job_mode(job, &owner, None, None, None, None, mode).await? {
        RunOutcome::Held => {
            if article_image.is_some() || checks {
                dev_hold_card(&repo, id, &owner, &ffmpeg, article_image.as_deref(), checks).await?;
            }
            anyhow::ensure!(finish_held(&repo, id, &owner)?, "job #{id} is no longer ours");
            println!("Held job #{id}");
        }
        RunOutcome::Delivered => anyhow::bail!("job #{id} was delivered instead of held"),
    }
    Ok(())
}

/// Invented sites, canaries, recipes and repair attempts for the admin cards
/// (P10.15a). Every name is `*.dev-seed.example`; running it again adds more
/// attempts and merges the recipes.
#[cfg(debug_assertions)]
fn dev_seed_repair(repo: &Repository) -> Result<()> {
    use omni_core::recipe_score::{Event, InsertOutcome, NewRecipe, Origin, ScopeKind};
    use omni_core::site_health::{SiteOutcome, SiteState};

    let now = chrono::Utc::now();
    let hours = chrono::Duration::hours;
    let days = chrono::Duration::days;

    // Sites: one broken with a canary that fails, one degraded, one healthy.
    for (domain, fails, state) in [
        ("broken.dev-seed.example", 4, Some(SiteState::Broken)),
        ("slow.dev-seed.example", 2, Some(SiteState::Degraded)),
        ("calm.dev-seed.example", 0, None),
    ] {
        for _ in 0..3 {
            repo.record_site_outcome(domain, SiteOutcome::Success)?;
        }
        for _ in 0..fails {
            repo.record_site_outcome(domain, SiteOutcome::SiteFailure("NO_VIDEO_FOUND".into()))?;
        }
        if let Some(s) = state {
            repo.set_site_state(domain, s)?;
        }
        for i in 0..3 {
            let page = format!("https://{domain}/story/{}", i + 1);
            repo.add_canary(domain, &page, None, now - days(i as i64 + 1))?;
        }
        let ids: Vec<i64> = repo.list_canaries_for(domain)?.into_iter().map(|c| c.id).collect();
        for (id, ok) in ids.into_iter().zip([fails == 0, false, false]) {
            repo.record_canary_result(id, ok, if ok { "ok" } else { "no video found on the page" })?;
        }
    }

    // Recipes: an admin one with approvals, a model one still untried, a player one.
    let make = |scope: ScopeKind, key: &str, family: Option<&str>, json: &str, origin: Origin| -> Result<NewRecipe> {
        let r = omni_browser::recipe::Recipe::from_json(json).map_err(|e| anyhow::anyhow!("seed recipe: {e}"))?;
        Ok(NewRecipe {
            scope_kind: scope,
            scope_key: key.to_string(),
            family: family.map(str::to_string),
            hash: omni_browser::recipe::scope_hash(scope.as_str(), key, &r),
            steps_hash: omni_browser::recipe::steps_hash(&r),
            recipe_json: r.canonical_json(),
            dsl_version: r.dsl_version,
            origin,
        })
    };
    let attr = r#"{"dsl_version":1,"steps":[{"op":"attr","css":"video","attr":"data-src"},{"op":"abs","from":0},{"op":"media","from":1,"referer":"page"}]}"#;
    let other = r#"{"dsl_version":1,"steps":[{"op":"attr","css":"div.player","attr":"data-video"},{"op":"abs","from":0},{"op":"media","from":1,"referer":"page"}]}"#;
    let mut ids = Vec::new();
    for new in [
        make(ScopeKind::Site, "broken.dev-seed.example", Some("jwplayer"), attr, Origin::Admin)?,
        make(ScopeKind::Site, "slow.dev-seed.example", None, other, Origin::Llm)?,
        make(ScopeKind::Player, "jwplayer", Some("jwplayer"), attr, Origin::Admin)?,
    ] {
        match repo.insert_recipe(&new, now)? {
            InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) | InsertOutcome::Revived(id) => ids.push(id),
            other => println!("A seed recipe was not stored: {other:?}"),
        }
    }
    if let [first, second, ..] = ids[..] {
        repo.recipe_event(first, "broken.dev-seed.example", Event::Approval, now - days(5))?;
        repo.recipe_event(first, "broken.dev-seed.example", Event::Approval, now - days(2))?;
        repo.recipe_event(first, "calm.dev-seed.example", Event::AutoDelivered, now - days(1))?;
        repo.recipe_event(second, "slow.dev-seed.example", Event::CanaryFail, now - days(1))?;
    }

    // Repair attempts (hold and shadow) on the broken site and the slow one.
    let candidates = |url: &str, length_ok: bool| {
        serde_json::json!([{
            "url": url, "origin": "recipe:attr", "duration_secs": 62.0,
            "checks": [
                {"name": "probe", "ok": true, "detail": "62 δλ, 720p (cdn.dev-seed.example)"},
                {"name": "length", "ok": length_ok, "detail": "x"},
            ],
        }])
        .to_string()
    };
    let nonce = now.timestamp_nanos_opt().unwrap_or_default();
    // (domain, mode, outcome, decision, image distance, length ok, expected secs, ms, hours ago)
    let runs: [(&str, &str, &str, Option<&str>, Option<i64>, bool, Option<f64>, i64, i64); 7] = [
        ("broken.dev-seed.example", "hold", "held", Some("approved"), Some(3), true, Some(62.0), 41_000, 30),
        ("broken.dev-seed.example", "hold", "held", Some("rejected"), Some(31), false, Some(180.0), 52_000, 72),
        ("broken.dev-seed.example", "hold", "held", None, Some(8), true, Some(61.0), 38_000, 2),
        ("broken.dev-seed.example", "hold", "nothing_verified", None, None, false, None, 90_000, 120),
        ("slow.dev-seed.example", "shadow", "shadow_found", None, None, true, Some(62.0), 33_000, 48),
        ("slow.dev-seed.example", "hold", "held", Some("expired"), None, false, None, 47_000, 200),
        ("calm.dev-seed.example", "hold", "exhausted", None, None, false, None, 120_000, 400),
    ];
    for (i, (domain, mode, outcome, decision, distance, length_ok, expected, ms, ago)) in runs.into_iter().enumerate() {
        let mut new = omni_core::models::NewJob::new(format!("attachment://dev-seed/{nonce}/{i}"), format!("{}_DEVSEED_TEST", i + 1), "DEVSEED".to_string());
        new.keyword = "DEVSEED".into();
        new.status = JobStatus::ManualDownload;
        let job = repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        let at = now - hours(ago);
        let attempt = repo.start_attempt(job, mode, at)?;
        let url = format!("https://cdn.dev-seed.example/v/{i}.mp4");
        let found = outcome == "held" || outcome == "shadow_found";
        let end = omni_core::repository::AttemptEnd {
            rung: Some("recipes".into()),
            outcome: Some(outcome.into()),
            domain: Some(domain.into()),
            candidates_json: found.then(|| candidates(&url, length_ok)),
            held_url: (outcome == "held").then(|| url.clone()),
            expected_secs: expected,
            duration_ms: Some(ms),
            llm_calls: i64::from(i % 3 == 0),
            tokens_in: if i % 3 == 0 { 1800 } else { 0 },
            tokens_out: if i % 3 == 0 { 120 } else { 0 },
            ..Default::default()
        };
        repo.finish_attempt(attempt, &end, at + chrono::Duration::milliseconds(ms))?;
        repo.dev_decide_attempt(attempt, decision, distance)?;
    }
    repo.record_selfheal("broken.dev-seed.example", "nothing", None, None, 2, None, now - hours(20))?;
    repo.record_selfheal("broken.dev-seed.example", "found", Some("0123456789abcdef0123456789abcdef"), Some("llm"), 2, None, now - hours(44))?;
    Ok(())
}

/// What a repaired hold would have recorded, so the card can be seen on a dev
/// instance: an invented checklist and a held attempt on an invented host, and
/// the same card evidence step the ladder runs (a local picture instead of a fetch).
#[cfg(debug_assertions)]
async fn dev_hold_card(
    repo: &Repository,
    id: i64,
    owner: &str,
    ffmpeg: &std::path::Path,
    article_image: Option<&std::path::Path>,
    checks: bool,
) -> Result<()> {
    use omni_core::recipe_score::{Event, InsertOutcome, NewRecipe, Origin, ScopeKind};

    let held = repo.waiting_held(id)?.context("no held row to describe")?;
    let dir = std::path::Path::new(&held.path).parent().context("the held file has no directory")?.to_path_buf();
    let duration = held.duration.unwrap_or(0.0);
    let now = chrono::Utc::now();
    let list = if checks {
        // Two more verified candidates after the held one: the card offers «Επόμενη επιλογή (2)».
        let more = |name: &str| {
            serde_json::json!({
                "url": format!("https://cdn.dev-hold.example/video/{name}.mp4"),
                "referer": "https://news.dev-hold.example/story/1",
                "origin": "recipe:dev0hold",
                "duration_secs": duration,
                "score": 5,
                "checks": [{"name": "probe", "ok": true, "detail": format!("{duration:.0} δλ, 720p (cdn.dev-hold.example)")}],
            })
        };
        let mut list = serde_json::json!([{
            "url": "https://cdn.dev-hold.example/video/clip.mp4",
            "referer": "https://news.dev-hold.example/story/1",
            "origin": "recipe:dev0hold",
            "duration_secs": duration,
            "score": 10,
            "checks": [
                {"name": "egress", "ok": true, "detail": "Δημόσιος κόμβος cdn.dev-hold.example της σελίδας"},
                {"name": "ad", "ok": true, "detail": "Δεν είναι διαφήμιση"},
                {"name": "probe", "ok": true, "detail": format!("{duration:.0} δλ, 720p (cdn.dev-hold.example)")},
                {"name": "length", "ok": true, "detail": format!("Ταιριάζει με τα {:.0} δλ της σελίδας", duration + 2.0)},
            ],
        }]);
        if let Some(a) = list.as_array_mut() {
            a.push(more("clip2"));
            a.push(more("clip3"));
        }
        list
    } else {
        serde_json::json!([])
    };
    // A recipe MCR already approved twice on this invented domain.
    let recipe = NewRecipe {
        scope_kind: ScopeKind::Site,
        scope_key: "dev-hold.example".into(),
        family: None,
        recipe_json: "{}".into(),
        hash: "dev-hold-recipe".into(),
        steps_hash: "dev-hold-steps".into(),
        dsl_version: 1,
        origin: Origin::Admin,
    };
    let recipe_id = match repo.insert_recipe(&recipe, now)? {
        InsertOutcome::Inserted(r) | InsertOutcome::Merged(r) => Some(r),
        _ => None,
    };
    if let Some(r) = recipe_id {
        if repo.recipe_site_approvals(r, "dev-hold.example")? == 0 {
            repo.recipe_event(r, "dev-hold.example", Event::Approval, now)?;
            repo.recipe_event(r, "dev-hold.example", Event::Approval, now)?;
        }
    }
    let attempt = repo.start_attempt(id, "hold", now)?;
    let end = omni_core::repository::AttemptEnd {
        rung: Some("recipes".into()),
        outcome: Some("held".into()),
        recipe_id,
        recipe_hash: Some("dev-hold-recipe".into()),
        domain: Some("dev-hold.example".into()),
        candidates_json: Some(list.to_string()),
        duration_ms: Some(0),
        held_url: Some("https://cdn.dev-hold.example/video/clip.mp4".into()),
        next_index: 0,
        expected_secs: Some(duration + 2.0),
        ..Default::default()
    };
    repo.finish_attempt(attempt, &end, now)?;
    if let Some(img) = article_image {
        let t = std::time::Instant::now();
        let done = omni_broadcast::card::card_from_file(ffmpeg, &dir, img, duration).await;
        println!("Card evidence took {} ms", t.elapsed().as_millis());
        match done {
            Some((distance, frame_secs)) => {
                anyhow::ensure!(
                    repo.record_card_evidence(attempt, id, owner, distance.map(i64::from), frame_secs, Some("img.dev-hold.example"))?,
                    "job #{id} is no longer ours"
                );
                println!("Article image: distance {distance:?}, closest frame at {frame_secs:?} s");
            }
            None => println!("The article image could not be decoded; the card has none."),
        }
    }
    Ok(())
}

async fn mail_ingest(paths: &AppPaths, files: &[std::path::PathBuf]) -> Result<()> {
    use omni_email::watcher::Ingested;

    let mut config = AppConfig::load_from_file(&paths.config)
        .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
    config.adopt_secrets(&SecretStore::new(paths.resolve("data/secrets.bin")))?;
    let repo = Repository::new(paths.resolve(&config.database_path))?;
    let source = std::sync::Arc::new(omni_email::eml::EmlSource::from_files(files)?);
    let watcher = omni_email::EmailWatcher::with_source(config, repo.clone(), source.clone());

    for id in source.ids() {
        let mail = omni_email::source::MailSource::fetch_mail(source.as_ref(), &id).await?;
        let key = omni_email::watcher::mail_key(&mail);
        match watcher.ingest(&mail).await {
            Ok(Ingested::AlreadyHandled) => println!("{id}: already handled ({key}); skipped"),
            Ok(Ingested::Processed) => {
                let row = repo.get_processed_mail(&key)?;
                let queued: Vec<omni_email::watcher::QueuedFromMail> = row
                    .as_ref()
                    .and_then(|r| serde_json::from_str(&r.jobs_json).ok())
                    .unwrap_or_default();
                println!(
                    "{id}: {} — {} job(s){}",
                    row.map(|r| r.outcome).unwrap_or_default(),
                    queued.len(),
                    queued.iter().map(|q| format!("\n    {}  {}", q.slug, q.url)).collect::<String>()
                );
            }
            Err(e) => println!("{id}: not queued: {e:#}"),
        }
    }
    println!("\nThe running daemon downloads what was queued; the MCR desk shows it under Email.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // The installer runs from a release folder, which is not an install:
    // nothing is created or logged next to it (plan P9).
    match &cli.command {
        Some(Commands::Install { dir, watchfolder, port, account, yes, no_firewall, import }) => {
            return omni_cli::install::install(omni_cli::install::InstallOptions {
                dir: dir.clone(),
                watchfolder: watchfolder.clone(),
                port: *port,
                account: account.clone(),
                yes: *yes,
                no_firewall: *no_firewall,
                import: import.clone(),
                base_config: None,
                // `--account` names an account knowingly: as before (P13.1c).
                confirm_share_for_passwordless: false,
            })
            .await;
        }
        Some(Commands::Uninstall { dir }) => return omni_cli::install::uninstall(dir.clone()).await,
        _ => {}
    }

    // Anchor every relative path to the install directory exactly once, before
    // anything can accidentally resolve one against the CWD (plan P0.1).
    let paths = match &cli.root {
        Some(root) => AppPaths::with_root(root, &cli.config),
        None => AppPaths::discover(&cli.config),
    };
    paths
        .ensure_dirs()
        .with_context(|| format!("Failed creating the directory tree under {:?}", paths.root))?;

    // Logging is set up *after* paths, because the log directory is one of
    // them, and only the long-running commands write files: a `secrets list`
    // must not rotate or prune the log the running service is writing to
    // (plan P6.1, defect W-11).
    let _log_guard = init_logging(&cli, &paths)?;
    // The log's redaction set, so cookie jars (P3.5) can join it once the database is open.
    let log_redactions = _log_guard.as_ref().map(|g| g.redactions());
    info!("Install root: {:?}", paths.root);

    // The adblock cache must be absolute before any sniff can run.
    omni_browser::UnifiedAdBlocker::init(paths.data.join("adblock"));

    match cli.command {
        None | Some(Commands::Run) => {
            run_daemon(&paths, None, log_redactions).await?;
        }
        Some(Commands::RunService) => {
            let paths = paths.clone();
            omni_service::run_service(move |shutdown_rx| {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build Tokio runtime for Windows Service");
                rt.block_on(async {
                    if let Err(e) = run_daemon(&paths, Some(shutdown_rx), log_redactions).await {
                        error!("Daemon error inside Windows Service: {:?}", e);
                    }
                });
                // Dropping the runtime waits without limit for blocking tasks
                // (a stuck mail fetch); SCM must hear SERVICE_STOPPED within
                // the wait hint the stop reported (P2.15).
                rt.shutdown_timeout(Duration::from_secs(20));
            })?;
        }
        Some(Commands::Setup) => {
            omni_cli::run_setup_wizard(Some(&cli.config)).await?;
        }
        Some(Commands::Install { .. } | Commands::Uninstall { .. }) => unreachable!("handled before paths"),
        Some(Commands::Service { action }) => {
            let sub = match action {
                ServiceAction::Install { account } => {
                    // The config gives the watchfolder (can the account reach
                    // it?) and the folders that need rights.
                    let config = AppConfig::load_from_file(&paths.config).ok();
                    omni_cli::ServiceSubcommand::Install {
                        account,
                        paths: paths.clone(),
                        config,
                    }
                }
                ServiceAction::Uninstall => omni_cli::ServiceSubcommand::Uninstall,
                ServiceAction::Start => omni_cli::ServiceSubcommand::Start,
                ServiceAction::Stop => omni_cli::ServiceSubcommand::Stop,
                ServiceAction::Status => omni_cli::ServiceSubcommand::Status,
            };
            omni_cli::handle_service_command(sub).await?;
        }
        Some(Commands::Selfcheck) => {
            let config = AppConfig::load_from_file(&paths.config)
                .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
            let repo = Repository::new(paths.resolve(&config.database_path))?;
            let health = HealthState::new();
            let ytdl = DependencyManager::new(&paths.bin).ytdl_live();
            let ffmpeg = resolve_ffmpeg(&config, &DependencyManager::new(paths.resolve(&config.bin_dir)));
            let deps = DependencyManager::new(&paths.bin);
            if !ensure_deno(&deps, &health, config.ytdl_auto_update_nightly).await {
                println!("  ✗ Deno: {}", health.get(omni_core::health::checks::DENO).and_then(|c| c.detail).unwrap_or_default());
            } else {
                println!("  ✓ Deno {} (for YouTube)", deps.deno_version().unwrap_or_default());
            }
            println!("Checking the browser…");
            selfcheck::check_browser(&health).await;
            if let Some(c) = health.get(omni_core::health::checks::BROWSER) {
                println!("  {} {}", if c.state == omni_core::health::Health::Ok { "✓" } else { "✗" }, c.detail.unwrap_or_default());
            }
            let links = repo.list_selfcheck_links()?;
            println!("Checking {} link(s); each takes a few seconds…", links.len());
            let (mut working, mut unchecked) = (0, 0);
            for (link, outcome) in selfcheck::check_and_record(&repo, &ytdl, Some(&ffmpeg), &omni_broadcast::pacing::HostPacer::default(), &links).await {
                working += usize::from(outcome.ok);
                unchecked += usize::from(outcome.inconclusive);
                // Inconclusive: the site was limiting requests (P11.4); not counted either way.
                let mark = if outcome.ok { "✓" } else if outcome.inconclusive { "–" } else { "✗" };
                println!("  {mark} {:<32} {}", link.label, outcome.detail);
            }
            println!("{working} of {} links work; {unchecked} not checked this time.", links.len());
        }
        Some(Commands::Cookies { action }) => cookies_cli::run(&paths, action).await?,
        Some(Commands::Benchmark { action }) => {
            // Exit code 2: the run finished but a threshold failed.
            if !benchmark::cli(&paths, action).await? {
                std::process::exit(2);
            }
        }
        Some(Commands::BrowserTest { url }) => {
            println!("Testing stream sniffer on URL: {}", url);
            match StreamSniffer::extract_media_bundle(&url, 25).await {
                Ok(bundle) => {
                    println!("\n✓ Successfully sniffed video stream!");
                    println!("Primary Stream: {}", bundle.primary_stream);
                    if bundle.all_streams.len() > 1 {
                        println!("\nTotal Discovered Media Streams ({}):", bundle.all_streams.len());
                        for (i, s) in bundle.all_streams.iter().enumerate() {
                            println!("  [{}] {}", i + 1, s);
                        }
                    }
                    println!("\nSession Context:");
                    println!("  User-Agent: {}", bundle.user_agent);
                    println!("  Referer:    {}", bundle.referer);
                    if let Some(c) = bundle.cookies {
                        println!("  Cookies:    {} chars", c.len());
                    }
                    println!();
                }
                Err(e) => {
                    eprintln!("\n✗ Stream sniffing failed: {}", e);
                }
            }
        }
        Some(Commands::MailPreview { last, hours, eml, llm }) => {
            mail_preview(&paths, last, hours, eml, llm).await?;
        }
        Some(Commands::MailIngest { eml }) => {
            mail_ingest(&paths, &eml).await?;
        }
        #[cfg(debug_assertions)]
        Some(Commands::DevHold { file, slug, journalist, article_image, checks }) => {
            dev_hold(&paths, &file, slug, journalist, article_image, checks).await?;
        }
        #[cfg(debug_assertions)]
        Some(Commands::DevSelfhealReport { domain, outcome, origin, pages, hours_ago }) => {
            let config_given = std::env::args().any(|a| a == "-c" || a == "--config" || a.starts_with("--config="));
            let config = AppConfig::load_from_file(&paths.config)
                .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
            let db_path = paths.resolve(&config.database_path);
            dev_hold_guard(&paths, config_given, &db_path)?;
            let repo = Repository::new(&db_path)?;
            let found = outcome == "found";
            repo.record_selfheal(
                &domain,
                &outcome,
                found.then_some("0123456789abcdef0123456789abcdef"),
                found.then_some(origin.as_str()),
                if found { pages } else { 0 },
                None,
                chrono::Utc::now() - chrono::Duration::hours(hours_ago),
            )?;
            println!("Added a {outcome} self-heal report for {domain}.");
        }
        #[cfg(debug_assertions)]
        Some(Commands::DevSeedRepair) => {
            let config_given = std::env::args().any(|a| a == "-c" || a == "--config" || a.starts_with("--config="));
            let config = AppConfig::load_from_file(&paths.config)
                .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
            let db_path = paths.resolve(&config.database_path);
            dev_hold_guard(&paths, config_given, &db_path)?;
            dev_seed_repair(&Repository::new(&db_path)?)?;
            println!("Seeded invented sites, recipes and repair attempts.");
        }
        Some(Commands::Report { what }) => report_cli::run(&paths, what)?,
        Some(Commands::MailHistory { last }) => {
            let config = AppConfig::load_from_file(&paths.config)
                .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
            let repo = Repository::new(paths.resolve(&config.database_path))?;
            let rows = repo.list_processed_mail(last)?;
            if rows.is_empty() {
                println!("\nNo mail handled yet.");
            }
            let pending: Vec<String> = repo.pending_mail_reprocess()?.into_iter().map(|(k, _, _)| k).collect();
            for m in rows {
                let jobs = serde_json::from_str::<Vec<serde_json::Value>>(&m.jobs_json).map(|v| v.len()).unwrap_or(0);
                println!(
                    "{}  {:<12} {:>2} job(s)  {}{}\n    {}",
                    m.processed_at.map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default(),
                    m.outcome,
                    jobs,
                    m.subject.unwrap_or_default(),
                    if pending.contains(&m.internet_message_id) { "   [reprocess pending]" } else { "" },
                    m.internet_message_id
                );
            }
        }
        Some(Commands::MailReprocess { message_id, subject }) => {
            let config = AppConfig::load_from_file(&paths.config)
                .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
            let repo = Repository::new(paths.resolve(&config.database_path))?;
            let key = match (message_id, subject) {
                (Some(id), _) => id.trim().to_string(),
                (None, Some(part)) => {
                    let wanted = part.trim().to_lowercase();
                    let hits: Vec<_> = repo
                        .list_processed_mail(500)?
                        .into_iter()
                        .filter(|m| m.subject.as_deref().unwrap_or("").to_lowercase().contains(&wanted))
                        .collect();
                    match hits.len() {
                        0 => anyhow::bail!("No handled mail has `{}` in its subject. See `omni-ingest mail-history`.", part.trim()),
                        1 => hits[0].internet_message_id.clone(),
                        n => {
                            println!("{n} handled mails match; name one with --message-id:");
                            for m in hits {
                                println!("  {}  {}", m.internet_message_id, m.subject.unwrap_or_default());
                            }
                            return Ok(());
                        }
                    }
                }
                (None, None) => anyhow::bail!("Name the mail: --message-id <id> or --subject <part of it>."),
            };
            let m = repo.request_mail_reprocess(&key, "command line")?;
            let _ = repo.log_audit("INFO", "EMAIL", &format!("Reprocess of '{}' requested from the command line", m.subject.clone().unwrap_or_default()));
            println!(
                "✓ '{}' will be read again on the daemon's next poll (it must be running). Jobs it already has are not queued twice.",
                m.subject.unwrap_or_default()
            );
        }
        Some(Commands::Admin { action }) => {
            let config = AppConfig::load_from_file(&paths.config)
                .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
            let repo = Repository::new(paths.resolve(&config.database_path))?;
            let sub = match action {
                AdminAction::ListUsers => omni_cli::AdminSubcommand::ListUsers,
                AdminAction::ResetPassword { email } => omni_cli::AdminSubcommand::ResetPassword { email },
                AdminAction::Deactivate { email } => omni_cli::AdminSubcommand::SetActive { email, active: false },
                AdminAction::Activate { email } => omni_cli::AdminSubcommand::SetActive { email, active: true },
            };
            omni_cli::handle_admin_command(&repo, sub)?;
        }
        Some(Commands::Secrets { action }) => {
            let store = SecretStore::new(paths.resolve("data/secrets.bin"));
            let sub = match action {
                SecretAction::List => omni_cli::SecretSubcommand::List,
                SecretAction::Set { key } => omni_cli::SecretSubcommand::Set { key },
                SecretAction::Clear { key } => omni_cli::SecretSubcommand::Clear { key },
            };
            omni_cli::handle_secrets_command(&store, sub)?;
        }
        Some(Commands::Adblock { action }) => match action {
            AdblockAction::Update => {
                println!("Synchronizing HaGeZi & Greek AdBlock filter lists...");
                let blocker = omni_browser::UnifiedAdBlocker::global();
                match blocker.update_blocklists().await {
                    Ok(stats) => {
                        println!("\n✓ AdBlock lists updated successfully!");
                        println!("  Total Unique Blocked Domains: {}", stats.total_domains);
                        println!("  HaGeZi Rules Parsed:          {}", stats.hagezi_count);
                        println!("  Greek Rules Parsed:           {}", stats.greek_count);
                        println!();
                    }
                    Err(e) => eprintln!("\n✗ Failed updating blocklists: {}", e),
                }
            }
            AdblockAction::Status => {
                let blocker = omni_browser::UnifiedAdBlocker::global();
                let stats = blocker.stats();
                println!("\nUnified AdBlock Engine Status:");
                println!("  Active Blocked Domains: {}", stats.total_domains);
                println!("  HaGeZi Source:          https://raw.githubusercontent.com/hagezi/dns-blocklists/main/adblock/light.txt");
                println!("  Greek Source:           https://www.void.gr/kargig/void-gr-filters.txt\n");
            }
        },
    }

    Ok(())
}

async fn run_daemon(
    paths: &AppPaths,
    mut service_shutdown_rx: Option<tokio::sync::mpsc::Receiver<()>>,
    log_redactions: Option<Redactions>,
) -> Result<()> {

    info!("============================================================");
    info!("   OMNIDOWNLOADER BROADCAST INGEST ENGINE v1.0.0            ");
    info!("   Sony XDCAM HD422 PAL 1080i50 + RDD9 OP1a broadcast ingest   ");
    info!("============================================================");

    let config_path = paths.config.clone();
    let mut config = AppConfig::load_from_file(&config_path)
        .with_context(|| format!("Failed loading configuration from {:?}", config_path))?;

    // Secrets live in an encrypted store, not in config.json (plan P2.6).
    // If this deployment still has a plaintext mailbox password in the config,
    // it is moved now and the config rewritten without it — otherwise the
    // password stays readable to anyone who can read the install directory.
    let secret_store = SecretStore::new(paths.resolve("data/secrets.bin"));
    if config
        .adopt_secrets(&secret_store)
        .context("Failed initialising the secret store")?
    {
        config
            .save_to_file(&config_path)
            .context("Failed rewriting config.json without the plaintext secret")?;
        info!("Rewrote {:?} without the plaintext mailbox password", config_path);
    }
    // Console / development runs may take the Graph settings from the
    // environment (plan P4.9). Names only in the log, never values.
    let from_env = config.apply_env_overrides(|name| std::env::var(name).ok());
    if !from_env.is_empty() {
        info!("Graph settings taken from the environment: {}", from_env.join(", "));
    }
    let config = config;

    // Every one of these is absolute: relative entries resolve against the
    // install root, absolute and UNC entries (a playout share) pass through.
    let db_path = paths.resolve(&config.database_path);
    let temp_path = paths.resolve(&config.temp_path);
    let watchfolder_path = paths.resolve(&config.watchfolder_path);
    let bin_dir = paths.resolve(&config.bin_dir);

    tokio::fs::create_dir_all(&temp_path).await?;
    // A scratch cookie jar left by a crash (P3.5): nothing is using one yet.
    omni_broadcast::cookie_file::sweep_scratch(&temp_path);
    tokio::fs::create_dir_all(&watchfolder_path).await?;
    tokio::fs::create_dir_all(&bin_dir).await?;

    info!("Initializing SQLite database at: {:?}", db_path);
    // One live-update bus: the repository announces job changes on it, the web server streams it (P1.10).
    let events = omni_core::events::EventBus::new();
    let mut repo = Repository::new(&db_path)?.with_events(events.clone());
    // Cookie jars are logins (P3.5): their values join the log redactor before anything uses them,
    // and every later read of a jar registers its values again (a jar the sign-in command stored).
    if let Some(r) = &log_redactions {
        repo = repo.with_cookie_redactions(r.clone());
        r.extend(repo.cookie_values());
    }
    // A brand-new installation starts with Admin / Admin (owner's decision,
    // 2026-10-08). First, before anything writes to the audit log, which
    // is how a used database is told apart from a new one.
    match repo.ensure_default_admin() {
        Ok(true) => warn!(
            "New installation: log in as Admin / Admin at http://127.0.0.1:{}/admin, create your own administrator and deactivate Admin.",
            config.web_port
        ),
        Ok(false) => {}
        Err(e) => warn!("Could not create the first-run administrator: {e:#}"),
    }

    // The generic recipes that ship with this build (P10.10). A failure here
    // costs the shipped recipes, never the daemon.
    match repo.sync_shipped(&omni_browser::recipe::shipped_for_store(), chrono::Utc::now()) {
        Ok((added, removed)) if added + removed > 0 => info!("Shipped recipes: +{added} -{removed}"),
        Ok(_) => {}
        Err(e) => warn!("Shipped recipes not synced: {e:#}"),
    }

    // Sessions that expired while the daemon was down are dead rows; clearing
    // them at start-up keeps the table from growing without bound on a machine
    // that is restarted far more often than it is logged into.
    match repo.purge_expired_sessions() {
        Ok(0) => {}
        Ok(n) => info!("Cleared {n} expired session(s)"),
        Err(e) => warn!("Could not clear expired sessions: {e:?}"),
    }
    if !repo.has_active_admin().unwrap_or(true) {
        warn!(
            "No administrator account exists. Open http://127.0.0.1:{}/setup from this machine, \
             or run `omni-ingest setup`, to create one.",
            config.web_port
        );
    }

    // Dependency manager and tool paths
    let dep_mgr = DependencyManager::new(&bin_dir);
    // A swap that died part-way is finished forward when its staged files match the pin, else
    // discarded; a .prev is never put back (P9.3).
    dep_mgr.recover_interrupted_swaps();

    // Keeps the nightly tool update from swapping yt-dlp.exe out from under a
    // running download (plan P2.9, defect D-16).
    let update_gate = UpdateGate::new();
    // Shared with the admin panel's installs and the «pinned tools» health line.
    let tool_installer = omni_core::dependencies::ToolInstaller::new();

    // A pinned tool missing from bin/ is installed at its pinned SHA-256 now, or the queue is
    // held (the encoder self-test fails on the absent file): never whatever is on PATH (P9.6).
    // One 180 s budget for all (the upgrade installer's /api/health wait is 4 minutes), smallest
    // download first; each result is logged and audited as it arrives. A service Stop during
    // this wait ends start-up at once.
    {
        let overrides: std::collections::HashMap<String, PathBuf> = [
            ("ffmpeg", &config.ffmpeg_path),
            ("ffprobe", &config.ffprobe_path),
            ("bmxtranswrap", &config.bmxtranswrap_path),
        ]
        .into_iter()
        .filter_map(|(n, p)| p.as_ref().map(|p| (n.to_string(), PathBuf::from(p))))
        .collect();
        let install = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
            for pin in dep_mgr.pending_pins(&overrides) {
                let tool = pin.tool;
                let attempt = tokio::time::timeout_at(
                    deadline,
                    dep_mgr.install_pinned(&pin, &omni_core::dependencies::HttpFetcher, &update_gate, Duration::from_secs(1), &tool_installer),
                )
                .await;
                match attempt {
                    Ok(Ok(out)) => {
                        info!("Start-up install of pinned {tool}: {}", out.message);
                        let _ = repo.log_audit("INFO", "TOOLS", &format!("Pinned {tool} was missing at start-up and was installed: {}", out.message));
                    }
                    Ok(Err(e)) => {
                        error!("Start-up install of pinned {tool} failed: {e:#}");
                        let _ = repo.log_audit("ERROR", "TOOLS", &format!("Pinned {tool} is missing and its install failed; the queue is held: {e:#}"));
                    }
                    Err(_) => {
                        error!("Start-up install of pinned {tool} did not finish in the 180 s budget");
                        let _ = repo.log_audit("ERROR", "TOOLS", &format!("Pinned {tool} is missing and its install did not finish in the 180 s start-up budget; the queue is held"));
                    }
                }
            }
        };
        let stop = async {
            match service_shutdown_rx.as_mut() {
                Some(rx) => {
                    rx.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = install => {}
            _ = stop => {
                info!("Windows Service Control Manager shutdown requested during the start-up tool install.");
                return Ok(());
            }
        }
    }
    if config.ytdl_path.is_none() {
        ensure_ytdl_folder_build(&dep_mgr, &config.ytdl_channel, config.ytdl_auto_update_nightly).await;
    }
    let ffmpeg_path = resolve_ffmpeg(&config, &dep_mgr);
    let ffprobe_path = config
        .ffprobe_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dep_mgr.find_binary("ffprobe"))
        .unwrap_or_else(|| dep_mgr.unresolved_path("ffprobe"));
    let bmxtranswrap_path = config
        .bmxtranswrap_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dep_mgr.find_binary("bmxtranswrap"))
        .unwrap_or_else(|| dep_mgr.unresolved_path("bmxtranswrap"));
    let ytdl_path = config
        .ytdl_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dep_mgr.find_binary("yt-dlp"))
        .unwrap_or_else(|| PathBuf::from("yt-dlp.exe"));
    // Tools that were not in bin/ now: the paths above are fixed for this run, so
    // an install from the admin panel is only used after a restart (P9.3).
    let tools_missing_at_start: Vec<String> = [
        ("ffmpeg", config.ffmpeg_path.is_none() && !bin_dir.join("ffmpeg.exe").is_file()),
        ("ffprobe", config.ffprobe_path.is_none() && !bin_dir.join("ffprobe.exe").is_file()),
        ("bmxtranswrap", config.bmxtranswrap_path.is_none() && !bin_dir.join("bmxtranswrap.exe").is_file()),
        ("yt-dlp", config.ytdl_path.is_none() && !dep_mgr.ytdl_live().is_file()),
    ]
    .into_iter()
    .filter(|(_, missing)| *missing)
    .map(|(name, _)| name.to_string())
    .collect();

    // The paths config.json sets: what the daemon runs, whatever bin/ holds.
    let tool_overrides: Vec<(String, PathBuf)> = [
        ("ffmpeg", &config.ffmpeg_path),
        ("ffprobe", &config.ffprobe_path),
        ("bmxtranswrap", &config.bmxtranswrap_path),
        ("yt-dlp", &config.ytdl_path),
    ]
    .into_iter()
    .filter_map(|(name, p)| p.as_ref().map(|p| (name.to_string(), PathBuf::from(p))))
    .collect();

    info!("Broadcast Toolchain:");
    {
        let arch = omni_core::toolpins::host_arch();
        info!("  host:         {} (tools pinned for {})", arch.label().to_lowercase(), arch.label().to_lowercase());
    }
    info!("  ffmpeg:       {:?}", ffmpeg_path);
    info!("  ffprobe:      {:?}", ffprobe_path);
    info!("  bmxtranswrap: {:?}", bmxtranswrap_path);
    info!("  yt-dlp:       {:?}", ytdl_path);
    match deno_banner_line(&dep_mgr) {
        (true, line) => info!("{line}"),
        (false, line) => warn!("{line}"),
    }

    // The benchmark (P6.4) runs the same tools beside the workers, never through them.
    let bench_tools = benchmark::Tools {
        ytdl: ytdl_path.clone(),
        ffmpeg: ffmpeg_path.clone(),
        ffprobe: ffprobe_path.clone(),
        bmxtranswrap: bmxtranswrap_path.clone(),
    };
    benchmark::startup_sweep(&repo, paths);
    if let Err(e) = repo.seed_benchmark_targets() {
        warn!("Benchmark corpus not stored: {e:#}");
    }
    // Delivered sources kept for a while (P1.7); `None` when switched off.
    let archive_settings = omni_core::archive::ArchiveSettings::from_config(&config.archive, paths, &watchfolder_path);
    if archive_settings.conflict {
        error!(
            "Archive: {:?} and the watchfolder {:?} contain one another; the archive is OFF and nothing in it is purged",
            archive_settings.root, watchfolder_path
        );
    }
    // Whether the archive has a volume of its own (neither temp's nor the
    // watchfolder's), and whether it shares temp's.
    let archive_separate = !omni_core::archive::same_volume(&archive_settings.root, &temp_path)
        && !omni_core::archive::same_volume(&archive_settings.root, &watchfolder_path);
    let archive_shares_temp = omni_core::archive::same_volume(&archive_settings.root, &temp_path);
    let repair_ffprobe = ffprobe_path.clone();
    let repair_ffmpeg = ffmpeg_path.clone();
    // Handed to the yt-dlp runs outside the engine: canaries, self-check, cookie tests (P11.4e).
    let ytdl_ffmpeg = ffmpeg_path.clone();
    let (st_ffmpeg, st_ffprobe, st_bmx) = (ffmpeg_path.clone(), ffprobe_path.clone(), bmxtranswrap_path.clone());
    // Whether the watchfolder answers (P11.2a): the start-up probe sets it, the
    // share watch and every delivery keep it current, the worker loop reads it.
    let watchfolder_state = omni_core::selftest::WatchfolderState::default();
    let broadcast_engine = Arc::new(BroadcastEngine::new(
        repo.clone(),
        ytdl_path.clone(),
        ffmpeg_path,
        ffprobe_path,
        bmxtranswrap_path,
        temp_path.clone(),
        watchfolder_path.clone(),
    )
    .with_encoder_slots(config.max_concurrent_transcodes.clamp(1, 8))
    .with_archive(Some(archive_settings.clone()))
    .with_watchfolder_state(watchfolder_state.clone())
    .with_retry_max_age(config.retry.max_age_mins)
    // One pacer for every yt-dlp run of the daemon (P11.4): jobs, the repair ladder,
    // the canaries, the self-check and the benchmark take turns at a platform.
    .with_pacing(config.pacing.per_host_concurrency, config.pacing.min_gap_secs));

    let (shutdown_tx, _) = broadcast::channel::<()>(16);

    // Live subsystem health, shared with the web panels (plan P6.2, W-09).
    let health = HealthState::new();

    // Everything the self-test finds used to surface as a *job* failure,
    // attributed to whatever link happened to be at the front of the queue —
    // a missing bmxtranswrap.exe reported as a problem with someone's YouTube
    // URL, once per job. It never blocks start-up: refusing to come up is the
    // one outcome an operator cannot diagnose from the MCR desk.
    omni_core::selftest::run(&health, &bin_dir, &watchfolder_path, &temp_path).await;
    watchfolder_state.set(
        health
            .get(omni_core::health::checks::WATCHFOLDER)
            .is_some_and(|c| c.state == omni_core::health::Health::Ok),
    );
    omni_core::selftest::check_default_admin(&repo, &health);
    omni_core::selftest::check_archive(
        &health,
        &archive_settings,
        archive_separate,
        omni_core::selftest::disk_space(&archive_settings.root).map(|(free, _)| free),
    );

    // Tools that start are not tools that make the file: an FFmpeg 9 build
    // answered `-version` and then rejected an output option, so every job
    // failed at transcode under a green "tools" check. One real 2 s job —
    // transcode, RDD9 rewrap, compliance gate — proves the chain.
    let encoder_ok = Arc::new(std::sync::atomic::AtomicBool::new(false));
    match omni_broadcast::selftest::encoder_self_test(&st_ffmpeg, &st_ffprobe, &st_bmx, &temp_path).await {
        Ok(report) => {
            encoder_ok.store(true, std::sync::atomic::Ordering::SeqCst);
            // Which H.264 encoder MCR's preview of a held video will use (P10.6).
            let preview_encoder = broadcast_engine.preview_encoder().await;
            health.set(
                omni_core::health::checks::ENCODER,
                omni_core::health::Check::ok(format!(
                    "test clip transcoded, rewrapped and verified in {:.1} s; preview: {}",
                    report.elapsed.as_secs_f64(),
                    preview_encoder.label()
                )),
            );
        }
        Err(e) => {
            error!("Encoder self-test failed: {e:#}. Jobs are held in the queue until this is fixed and the daemon restarted.");
            let _ = repo.log_audit("ERROR", "SELFTEST", &format!("Encoder self-test failed: {e}"));
            health.set(
                omni_core::health::checks::ENCODER,
                omni_core::health::Check::down(format!("{e} — jobs are held; fix the tool and restart")),
            );
        }
    }

    // Deno for YouTube (plan P6.8): reported now, and fetched in the
    // background when it is missing, so a fresh install or a deleted
    // bin/deno.exe repairs itself instead of failing YouTube jobs.
    {
        let health = health.clone();
        let bin_dir = bin_dir.clone();
        let auto = config.ytdl_auto_update_nightly;
        tokio::spawn(async move {
            ensure_deno(&DependencyManager::new(&bin_dir), &health, auto).await;
        });
    }

    // The self-check's last results, and a browser launch in the background:
    // a broken browser is reported now, not when the first article fails.
    selfcheck::refresh_health(&repo, &health, &bin_dir);
    omni_core::site_health::refresh_health(&repo, &health);
    {
        let health = health.clone();
        tokio::spawn(async move {
            selfcheck::check_browser(&health).await;
        });
    }

    match health.overall() {
        omni_core::health::Health::Ok => info!("Start-up self-test passed"),
        verdict => {
            for (name, check) in health.all() {
                if check.state != omni_core::health::Health::Ok {
                    warn!(
                        "Self-test [{name}]: {} — {}",
                        check.state.as_str(),
                        check.detail.as_deref().unwrap_or("no detail")
                    );
                }
            }
            warn!(
                "Start-up self-test reports {}; the daemon is running and the panel shows detail",
                verdict.as_str()
            );
        }
    }

    // 1. Start Embedded Web Server
    // One LLM assist for the watcher, the admin panel and the health check;
    // the panel swaps it when the settings are saved (plan P4.22).
    let live_llm = omni_email::assist::LiveAssist::new(omni_email::assist::Assist::from_config(&config));
    // Jobs a worker is still busy with: a retry from MCR waits for them.
    let busy_jobs = omni_core::busy::BusyJobs::new();
    // Site canaries (plan P10.4): shared by the nightly check, the workers'
    // daytime trigger and the admin's «Run now», so only one runs at a time.
    let canary_env = Arc::new(
        canary::CanaryEnv::new(repo.clone(), health.clone(), bin_dir.clone(), config.ytdl_channel.clone())
            .with_scratch(temp_path.clone())
            .with_pacer(broadcast_engine.pacer())
            .with_ffmpeg(ytdl_ffmpeg.clone()),
    );
    // The admin's Test URL: a dry run of the ladder; its world is set once the workers' exists.
    let url_tester = Arc::new(repair::AdminTester::new());
    omni_core::cookies::refresh_health(&repo, &health);
    // Only the Windows service locks its folders; a console run would lock the developer out.
    lock_down_install(service_shutdown_rx.is_some(), &[paths.data.clone(), temp_path.clone()], &health).await;
    // The «pinned tools» health line; the panel's installs refresh it and share the hash cache.
    {
        let override_map: std::collections::HashMap<String, PathBuf> = tool_overrides.iter().cloned().collect();
        let (mgr, st) = (DependencyManager::new(&bin_dir), tool_installer.clone());
        if let Ok(check) = tokio::task::spawn_blocking(move || mgr.pinned_tools_check(&st, &override_map)).await {
            health.set(omni_core::health::checks::PINNED_TOOLS, check);
        }
    }
    // The Prometheus recorder (P6.5): installed once, and only when switched on.
    // Off, nothing is recorded and `/metrics` is a 404.
    let metrics_handle = if config.metrics.enabled { install_metrics_recorder() } else { None };
    if let Some(handle) = metrics_handle.clone() {
        // Histograms need their housekeeping now and then, or they grow without bound.
        let mut upkeep_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = upkeep_rx.recv() => break,
                    _ = tick.tick() => handle.run_upkeep(),
                }
            }
        });
    }
    let mut web_state = AppState::new(repo.clone(), config.clone(), config_path.to_path_buf())
        .with_url_tester(url_tester.clone())
        .with_cookie_tester(Arc::new(CookieJarTester {
            repo: repo.clone(),
            bin_dir: bin_dir.clone(),
            temp_path: temp_path.clone(),
            pacer: broadcast_engine.pacer(),
            ffmpeg: Some(ytdl_ffmpeg),
        }))
        .with_site_runner(Arc::new(canary::AdminRunner::new(canary_env.clone())))
        .with_benchmark_starter(Arc::new(benchmark::Starter::new(Arc::new(benchmark::Benchmark::new(
            repo.clone(),
            paths.clone(),
            Arc::new(benchmark::RealStages::new(repo.clone(), bench_tools, paths.benchmark_temp()).with_pacer(broadcast_engine.pacer())),
            "daemon",
        ).with_gate(update_gate.clone())))))
        .with_secret_store(secret_store.clone())
        .with_health(health.clone())
        .with_llm(live_llm.clone())
        .with_busy_jobs(busy_jobs.clone())
        .with_update_gate(update_gate.clone())
        .with_tool_dirs(bin_dir.clone(), tools_missing_at_start)
        .with_tool_overrides(tool_overrides)
        .with_tool_installer(tool_installer)
        .with_canary_lock(canary_env.run_lock_handle())
        .with_temp_dir(temp_path.clone())
        .with_events(events.clone());
    if let Some(r) = &log_redactions {
        web_state = web_state.with_redactions(r.clone());
    }
    if let Some(handle) = metrics_handle {
        web_state = web_state.with_metrics(
            handle,
            omni_web::state::MetricsSources {
                temp: temp_path.clone(),
                watchfolder: watchfolder_path.clone(),
                archive: archive_settings.purges().then(|| archive_settings.root.clone()),
                workers_max: config.max_concurrent_downloads.clamp(1, 10) + config.max_concurrent_transcodes.clamp(1, 8),
            },
        );
    }
    web_state.end_default_admin_sessions();
    let web_host = config.web_host.clone();
    let web_port = config.web_port;
    let web_rx = shutdown_tx.subscribe();

    tokio::spawn(async move {
        if let Err(e) = WebServer::run(web_state, &web_host, web_port, web_rx).await {
            error!("Web server terminated with error: {:?}", e);
        }
    });

    // 2. Start Email Monitoring Watchdog
    // The Office 365 mailbox through Microsoft Graph; see EmailWatcher::new.
    if config.graph.is_configured() {
        let email_watcher = Arc::new(
            EmailWatcher::new(config.clone(), repo.clone())
                .with_attachments_dir(temp_path.join("attachments"))
                .with_health(health.clone())
                .with_live_assist(live_llm.clone()),
        );
        let email_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            email_watcher.start_polling_loop(email_rx).await;
        });
    } else {
        info!("Email monitoring disabled (the Graph mailbox is not configured: tenant id, client id, mailbox and client secret).");
        health.set(
            omni_core::health::checks::MAIL,
            omni_core::health::Check::disabled("Mailbox"),
        );
    }

    // 2b. LLM reachability.
    //
    // Polled rather than probed on demand, so the panel can say when it last
    // worked. Degraded, never down: the parser is deterministic-first and the
    // newsroom keeps running with the model offline — which is the whole point
    // of that constraint, and the status must not imply otherwise.
    {
        let llm_health = health.clone();
        let live = live_llm.clone();
        let mut llm_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = llm_rx.recv() => break,
                    _ = tick.tick() => {
                        // The current settings each time: the panel may
                        // have changed them since the last tick.
                        let assist = live.current();
                        let check = if assist.mode() == omni_core::config::LlmMode::Off {
                            omni_core::health::Check::disabled("LLM assist")
                        } else if assist.client().ping().await {
                            omni_core::health::Check::ok(assist.describe())
                        } else {
                            omni_core::health::Check::degraded(format!(
                                "{} unreachable or key refused; parsing continues without it",
                                assist.describe()
                            ))
                        };
                        llm_health.set_if_changed(omni_core::health::checks::LLM, check);
                    }
                }
            }
        });
    }


    // 3. Maintenance scheduler (plan P6.6).
    //
    // Replaces `if now.format("%H") == "03"` inside an hourly tick, which could
    // not express a time off the hour, could not notice a run missed while the
    // machine was off, and recorded nothing about whether last night worked.
    {
        let specs = omni_core::scheduler::default_tasks();
        repo.ensure_scheduled_tasks(&specs)
            .context("Failed registering the maintenance tasks")?;

        let repo_sched = repo.clone();
        let mut sched_rx = shutdown_tx.subscribe();
        let ctx = MaintenanceContext {
            bin_dir: bin_dir.clone(),
            temp_path: temp_path.clone(),
            paths: paths.clone(),
            ytdl_channel: config.ytdl_channel.clone(),
            ytdl_enabled: config.ytdl_auto_update_nightly,
            adblock_enabled: config.adblock_auto_update_nightly,
            gate: update_gate.clone(),
            retention_days: config.retention_days.max(1),
            archive: Some(archive_settings.clone()),
            mail_text_retention_days: config.mail_text_retention_days.max(1),
            health: health.clone(),
            canary: canary_env.clone(),
            evidence_days: config.repair.evidence_days.max(1),
            evidence_max_bytes: config.repair.evidence_max_mb.saturating_mul(1024 * 1024),
        };

        tokio::spawn(async move {
            // A minute is fine: these tasks are nightly, and the cost of a tick
            // is one indexed query against four rows.
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            let mut jars_seen = None;
            loop {
                tokio::select! {
                    _ = sched_rx.recv() => break,
                    _ = tick.tick() => {
                        omni_core::cookies::refresh_health_if_changed(&repo_sched, &ctx.health, &mut jars_seen);
                        run_due_tasks(&repo_sched, &specs, &ctx).await;
                    }
                }
            }
        });
    }

    // 4. Recover jobs this host was running when it last stopped.
    //
    // Must happen before any worker starts leasing (plan P1.1, defect D-02).
    // Without it a crash during a transcode strands the job in RUNNING forever:
    // no worker owns it and no lease anybody is watching will expire.
    let hostname = hostname_for_lease();
    // On a blocking thread: recovery looks at the watchfolder, which may be a slow share.
    let recovery = {
        let (repo, hostname) = (repo.clone(), hostname.clone());
        tokio::task::spawn_blocking(move || repo.recover_on_startup(&hostname)).await
    };
    match recovery {
        Ok(Ok(0)) => info!("No orphaned jobs to recover."),
        Ok(Ok(n)) => warn!("Recovered {n} orphaned job(s) from a previous run; requeued."),
        Ok(Err(e)) => error!("Orphan recovery failed: {e:?}"),
        Err(e) => error!("Orphan recovery did not finish: {e:?}"),
    }

    // 5. Sweep scratch directories left behind by jobs that are no longer
    // running. On a newsroom machine these are gigabytes of source media.
    // A finished video that waits for the watchfolder (P11.2a) is kept.
    match startup_sweep(&repo, &temp_path.join("jobs")).await {
        Ok(0) => {}
        Ok(removed) => info!("Swept {removed} orphaned job workspace(s) from temp/jobs."),
        Err(e) => warn!("Could not list running jobs for the temp sweep: {e:?}"),
    }

    // 5b. Leftover staging files of copies a crash interrupted, in the watchfolder
    // itself (top level, only our own `.name.mxf.tmp` and probe names, 10 minutes old).
    {
        let wf = watchfolder_path.clone();
        match tokio::task::spawn_blocking(move || omni_broadcast::delivery::sweep_stale_staging(&wf)).await {
            Ok(0) => {}
            Ok(n) => info!("Removed {n} stale staging file(s) from the watchfolder."),
            Err(e) => warn!("The watchfolder staging sweep did not finish: {e:?}"),
        }
    }

    // 6. Lease reaper: requeue jobs whose owner died without releasing them.
    {
        let repo_reaper = repo.clone();
        let queue_health = health.clone();
        let reaper_temp = temp_path.clone();
        let mut reaper_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(LEASE_REAP_INTERVAL);
            loop {
                tokio::select! {
                    _ = reaper_rx.recv() => break,
                    _ = interval.tick() => {
                        match repo_reaper.reap_expired_leases() {
                            Ok((0, 0)) => {}
                            Ok((requeued, review)) => warn!(
                                "Lease reaper: {requeued} job(s) requeued, {review} sent to review"
                            ),
                            Err(e) => error!("Lease reaper failed: {e:?}"),
                        }
                        // Videos held for approval (P10.5): expire, supersede, tidy.
                        let (sweep_repo, sweep_temp) = (repo_reaper.clone(), reaper_temp.clone());
                        match tokio::task::spawn_blocking(move || {
                            omni_core::held::sweep(&sweep_repo, &sweep_temp, chrono::Utc::now())
                        })
                        .await
                        {
                            Ok(Ok(r)) if r.is_empty() => {}
                            Ok(Ok(r)) => info!(
                                "Held-video sweep: {} expired, {} superseded, {} forgotten, {} stray folder(s) removed",
                                r.expired.len(), r.superseded.len(), r.forgotten.len(), r.dirs_removed
                            ),
                            Ok(Err(e)) => warn!("Held-video sweep failed: {e:?}"),
                            Err(e) => warn!("Held-video sweep did not finish: {e:?}"),
                        }
                        match repo_reaper.queue_watch(chrono::Utc::now()) {
                            Ok(w) => {
                                let was_degraded = queue_health
                                    .get(omni_core::health::checks::QUEUE)
                                    .is_some_and(|c| c.state == omni_core::health::Health::Degraded);
                                queue_health.set_if_changed(
                                    omni_core::health::checks::QUEUE,
                                    queue_check(&w, was_degraded),
                                );
                            }
                            Err(e) => warn!("Queue check could not read the queue: {e:?}"),
                        }
                    }
                }
            }
        });
    }

    // 6b. Share watch (P11.2a): the watchfolder is looked at every 30 s, so a
    // video that waits for it is delivered soon after it answers again, and the
    // `watchfolder` line says while it does not. While it answers the look is
    // read-only (nothing is written into the watchfolder); once it does not (a
    // failed look, a failed start-up probe, a delivery that found it
    // unreachable) the write probe runs, and only a passing one reopens it. A
    // look that does not answer in 15 s counts as not reachable; one still
    // hanging is not started twice.
    {
        let (state, wf_health, wf) = (watchfolder_state.clone(), health.clone(), watchfolder_path.clone());
        let mut watch_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            use omni_core::selftest::ShareCheck;
            let mut tick = tokio::time::interval(omni_core::selftest::WATCHFOLDER_PROBE_EVERY);
            tick.tick().await; // the start-up self-test has just probed
            let mut probe: Option<(ShareCheck, tokio::task::JoinHandle<Result<(), String>>)> = None;
            loop {
                tokio::select! {
                    _ = watch_rx.recv() => break,
                    _ = tick.tick() => {
                        let (check, mut running) = match probe.take() {
                            Some((check, p)) if !p.is_finished() => (check, p),
                            _ => {
                                let (path, check) = (wf.clone(), ShareCheck::for_state(state.reachable()));
                                (check, tokio::task::spawn_blocking(move || check.run(&path)))
                            }
                        };
                        let answered = tokio::time::timeout(omni_core::selftest::WATCHFOLDER_PROBE_TIMEOUT, &mut running).await;
                        let result = match answered {
                            Ok(Ok(r)) => Some(r),
                            Ok(Err(e)) => Some(Err(format!("the check did not finish: {e}"))),
                            Err(_) => {
                                probe = Some((check, running));
                                None
                            }
                        };
                        let was = state.reachable();
                        let now = omni_core::selftest::record_watchfolder(&wf_health, &state, &wf, check, result.as_ref());
                        if was != now {
                            if now {
                                info!("The watchfolder answers again; videos waiting for it are delivered");
                            } else {
                                warn!("The watchfolder does not answer; finished videos wait for it");
                            }
                        }
                    }
                }
            }
        });
    }

    // 7. Worker pool.
    //
    // Capacity is acquired *before* leasing (plan P1.1, defect D-01). The old
    // loop leased first and then waited on the semaphore, so with two workers
    // and twenty pending jobs, eighteen sat in DOWNLOADING with nobody working
    // on them -- and the MCR panel showed eighteen phantom downloads while the
    // operator waited for files that were not being made.
    //
    // Two limits (plan P1.13): `max_concurrent_downloads` jobs fetching
    // their source, and the engine's encoder slots. A job hands its download
    // slot back once its source is on disk, so the next download starts
    // while it waits for an encoder. Jobs in flight are capped at downloads
    // + encoders, so downloads run ahead of the encoders by a bounded amount.
    let max_concurrency = config.max_concurrent_downloads.clamp(1, 10);
    let max_encoders = config.max_concurrent_transcodes.clamp(1, 8);
    let semaphore = Arc::new(Semaphore::new(max_concurrency));
    let in_flight = Arc::new(Semaphore::new(max_concurrency + max_encoders));
    let mut worker_rx = shutdown_tx.subscribe();
    let repo_worker = repo.clone();
    let busy_worker = busy_jobs.clone();
    let engine_worker = broadcast_engine.clone();
    let worker_hostname = hostname.clone();
    let worker_gate = update_gate.clone();
    let worker_encoder_ok = encoder_ok.clone();
    let worker_health = health.clone();
    let worker_watchfolder = watchfolder_path.clone();
    let worker_watchfolder_state = watchfolder_state.clone();
    let worker_temp = temp_path.clone();
    let worker_archive = archive_settings.clone();
    // The LLM rung of the repair ladder (P10.12): both the switch and a model name,
    // never the mail model. Endpoint, key and auth are the `llm` section's.
    let repair_llm_cfg = repair::LlmCfg {
        enabled: config.repair.llm_enabled,
        model: config.repair.llm_model.clone().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()),
        timeout: Duration::from_secs(config.repair.llm_timeout_secs.clamp(5, 600)),
        max_calls_per_day: config.repair.llm_max_calls_per_day,
    };
    if config.repair.llm_enabled && repair_llm_cfg.model.is_none() {
        warn!("repair.llm_enabled is on but repair.llm_model is not set: the repair model rung stays off");
    }
    let mut repair_env = repair::RealEnv::new(broadcast_engine.clone(), repair_ffprobe.clone(), ytdl_path.clone(), repair_ffmpeg.clone());
    if let (true, Some(model)) = (repair_llm_cfg.active(), repair_llm_cfg.model.as_deref()) {
        let client_cfg = omni_core::config::LlmConfig { timeout_secs: repair_llm_cfg.timeout.as_secs(), ..config.llm.clone() };
        let client = omni_email::llm::LlmClient::from_settings(&config.ollama_endpoint, model, &client_cfg)
            .with_reasoning_effort(config.repair.llm_reasoning_effort.clone());
        info!("Repair model rung on: model {model}, at most {} call(s) a day", repair_llm_cfg.max_calls_per_day);
        repair_env = repair_env.with_model(repair::LlmModel::new(client, config.repair.llm_max_tokens));
    }
    let worker_repair = Arc::new(repair::RepairCtx {
        repo: repo.clone(),
        cfg: repair::RepairCfg {
            mode: config.repair.mode,
            budget: Duration::from_secs(config.repair.budget_secs),
            hold_hours: config.repair.hold_hours,
            held_max_bytes: config.repair.held_max_gb.saturating_mul(1024 * 1024 * 1024),
            auto_deliver_after_approvals: config.repair.auto_deliver_after_approvals.max(0),
            llm: repair_llm_cfg,
            egress_allow: config.security.egress_allow_networks(),
        },
        env: Arc::new(repair_env),
    });
    // The overnight self-heal (P10.14) probes with the same ladder world as the workers.
    canary_env.set_heal(worker_repair.clone());
    url_tester.set_ctx(worker_repair.clone());
    let worker_canary = canary_env.clone();
    // The job runs (P11.8): lease heartbeat, release on every exit, and the
    // `workers` line for runs that hit an internal error.
    let worker_ctx = worker::JobRunCtx::new(repo_worker.clone(), engine_worker.clone(), worker_health.clone());
    worker_health.set(omni_core::health::checks::WORKERS, omni_core::health::Check::ok("running"));

    tokio::spawn(async move {
        info!("Queue worker pool active ({max_concurrency} downloads, {max_encoders} encoders)");
        let mut worker_seq: u64 = 0;

        loop {
            // Capacity first. This await is where an idle daemon sits.
            let mut permit = tokio::select! {
                _ = worker_rx.recv() => {
                    info!("Worker pool shutting down.");
                    break;
                }
                p = async {
                    let job_slot = in_flight.clone().acquire_owned().await?;
                    // A download slot only if one is free right now: an
                    // approved held video is delivered without downloading
                    // and must not queue behind other people's downloads.
                    let download_slot = semaphore.clone().try_acquire_owned().ok();
                    Ok::<_, tokio::sync::AcquireError>((job_slot, download_slot))
                } => match p {
                    Ok(p) => p,
                    Err(_) => break,
                },
            };

            worker_seq += 1;

            // The start-up test could not make a compliant file with these
            // tools. Leasing now would only turn every waiting job into a
            // transcode failure for MCR to retry one by one; they wait instead.
            if !worker_encoder_ok.load(std::sync::atomic::Ordering::SeqCst) {
                drop(permit);
                tokio::select! {
                    _ = worker_rx.recv() => break,
                    _ = tokio::time::sleep(Duration::from_secs(10)) => continue,
                }
            }

            // Under STOP_DISK_GB free, no new job starts (plan P1.4): the
            // `disk` check goes red and the queue waits until there is room
            // again, rather than every job failing in turn mid-encode.
            // Short of room on the archive's own volume, or on temp's when they
            // are the same, the oldest archived days go first (P1.7): a kept
            // source must never be the reason a newsroom job waits. Runs with
            // the archive switched off too, so old archives still give way.
            if worker_archive.purges() {
                let (a, temp) = (worker_archive.clone(), worker_temp.clone());
                let low = (omni_core::selftest::LOW_DISK_GB * 1024.0 * 1024.0 * 1024.0) as u64;
                let shares = archive_shares_temp;
                let _ = tokio::task::spawn_blocking(move || {
                    let temp_free = || omni_core::selftest::disk_space(&temp).map(|(free, _)| free);
                    omni_core::archive::relieve_low_disk(
                        &a.root,
                        low,
                        || omni_core::selftest::disk_space(&a.root).map(|(free, _)| free),
                        shares.then_some(&temp_free as &dyn Fn() -> Option<u64>),
                    )
                })
                .await;
            }
            omni_core::selftest::check_archive(
                &worker_health,
                &worker_archive,
                archive_separate,
                omni_core::selftest::disk_space(&worker_archive.root).map(|(free, _)| free),
            );
            // A video that waits for the watchfolder (P11.2a) still goes then: it
            // needs no temp space, and delivering it is what frees some.
            let disk_ok = omni_core::selftest::check_disks(&worker_health, &worker_watchfolder, &worker_temp);

            // A tool swap is pending: hold off rather than lease a job whose
            // downloader is about to be replaced underneath it. The pass is
            // taken *before* leasing, so there is no window where a job is
            // owned but uncounted.
            let Some(gate_pass) = worker_gate.try_enter() else {
                drop(permit);
                tokio::select! {
                    _ = worker_rx.recv() => break,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                }
            };

            let owner = format!("{worker_hostname}:{}:{worker_seq}", std::process::id());
            // A video that waits for the watchfolder is leased only while it answers (P11.2a).
            let deliver_ok = worker_watchfolder_state.reachable();

            let leased = match lease_now(&repo_worker, &owner, disk_ok, permit.1.is_some(), deliver_ok) {
                // Every download slot is busy and nothing deliver-only is ready:
                // anything else waits briefly for a slot.
                Ok(None) if disk_ok && permit.1.is_none() => {
                    match tokio::time::timeout(Duration::from_secs(1), semaphore.clone().acquire_owned()).await {
                        Ok(Ok(slot)) => {
                            permit.1 = Some(slot);
                            repo_worker.lease_job_when(&owner, LEASE_SECS, worker_watchfolder_state.reachable())
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {
                            drop(permit);
                            continue;
                        }
                    }
                }
                other => other,
            };
            let job = match leased {
                Ok(Some(job)) => job,
                Ok(None) if !disk_ok => {
                    // Under STOP_DISK_GB and no waiting delivery: wait for room, as before.
                    drop(permit);
                    tokio::select! {
                        _ = worker_rx.recv() => break,
                        _ = tokio::time::sleep(Duration::from_secs(30)) => continue,
                    }
                }
                Ok(None) => {
                    // Nothing ready. Release the permit and idle with jitter so
                    // several workers do not wake in lockstep and hammer SQLite.
                    drop(permit);
                    // An internal error ages out of the `workers` line on a quiet day too.
                    worker_ctx.refresh_health();
                    let jitter = Duration::from_millis(1000 + (worker_seq % 5) * 100);
                    tokio::select! {
                        _ = worker_rx.recv() => break,
                        _ = tokio::time::sleep(jitter) => continue,
                    }
                }
                Err(e) => {
                    error!("Failed leasing a job: {e:?}");
                    drop(permit);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            worker_ctx.refresh_health();
            let ctx = worker_ctx.clone();
            let eng = engine_worker.clone();
            let rep = repo_worker.clone();
            let evidence_dir = worker_temp.join("evidence");
            let job_health = worker_health.clone();
            let job_canary = worker_canary.clone();
            let job_repair = worker_repair.clone();
            let (job_slot, download_slot) = permit;
            let leased = worker::LeasedJob {
                job_id: job.id,
                owner: owner.clone(),
                busy: busy_worker.guard(job.id, &owner),
                gate_pass,
                job_slot,
                download_slot,
            };

            tokio::spawn(async move {
                worker::run_leased_job(&ctx, leased, move |cancel| async move {
                    run_job(&rep, &eng, &owner, job, cancel, &evidence_dir, &job_repair, &job_health, &job_canary).await
                })
                .await;
            });
        }
    });

    info!("OmniDownloader daemon running. Press Ctrl+C to terminate.");

    // Wait for shutdown signal
    if let Some(mut rx) = service_shutdown_rx {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received. Initiating graceful shutdown...");
            }
            _ = rx.recv() => {
                info!("Windows Service Control Manager shutdown requested.");
            }
        }
    } else {
        let _ = tokio::signal::ctrl_c().await;
        info!("Ctrl+C received. Initiating graceful shutdown...");
    }

    // First, so no job still encoding starts a copy the stop below would cut.
    broadcast_engine.stop_new_deliveries();
    let _ = shutdown_tx.send(());
    tokio::time::sleep(Duration::from_millis(800)).await;
    // A job already delivering is let finish: dropped mid-copy it leaves its
    // `.slug.mxf.tmp`, which takes the slug from its own re-run (`_2`), and
    // dropped after the rename it is requeued and delivered twice (P2.15).
    // A database error counts as delivering: one failed read must not end the wait.
    let stuck = wait_for_deliveries(&busy_jobs, |id| match repo.get_job(id) {
        Ok(row) => row.map(|j| (j.status, j.stage)),
        Err(_) => Some((JobStatus::Running, JobStage::Deliver)),
    }, SHUTDOWN_DELIVERY_BUDGET).await;
    if stuck > 0 {
        warn!("{stuck} job(s) still delivering after {} s; stopping anyway", SHUTDOWN_DELIVERY_BUDGET.as_secs());
    }
    // The rest stay RUNNING under this host's lease: their tools die with the
    // process (job object) and the next start's recovery requeues them.
    let left = busy_jobs.len();
    if left > 0 {
        info!("{left} job(s) in flight are left to start-up recovery");
    }
    info!("OmniDownloader shutdown complete. Goodbye!");

    Ok(())
}

/// A ready video waiting, or one step running, longer than this is a warning.
const QUEUE_SLOW_SECS: i64 = 30 * 60;

/// Once degraded, the check recovers only when both measures are under this,
/// so a backlog hovering near 30 min does not alarm on every edge.
const QUEUE_RECOVER_SECS: i64 = 25 * 60;

/// Install the global metrics recorder for `GET /metrics` (P6.5). A failure is
/// logged and leaves the route a 404: the daemon runs the same without metrics.
fn install_metrics_recorder() -> Option<metrics_exporter_prometheus::PrometheusHandle> {
    use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
    let builder = match PrometheusBuilder::new().set_buckets_for_metric(
        Matcher::Full("omni_stage_duration_seconds".to_string()),
        &[1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0],
    ) {
        Ok(b) => b,
        Err(e) => {
            warn!("Metrics: bad histogram buckets, /metrics stays off: {e}");
            return None;
        }
    };
    match builder.install_recorder() {
        Ok(handle) => {
            info!("Metrics: GET /metrics is on");
            Some(handle)
        }
        Err(e) => {
            warn!("Metrics: could not start the recorder, /metrics stays off: {e}");
            None
        }
    }
}

/// The `queue` health check (plan P7.19). Never `down`: a slow queue is a warning.
fn queue_check(w: &omni_core::models::QueueWatch, was_degraded: bool) -> omni_core::health::Check {
    use omni_core::health::Check;
    let limit = if was_degraded { QUEUE_RECOVER_SECS } else { QUEUE_SLOW_SECS };
    let counts = format!("{} waiting, {} running", w.waiting, w.running);
    let mut problems = Vec::new();
    if let Some(secs) = w.oldest_ready_wait_secs.filter(|s| *s > limit) {
        problems.push(format!("a video has waited {} min", secs / 60));
    }
    if let Some((id, stage, secs)) = w.longest_step.as_ref().filter(|(_, _, s)| *s > limit) {
        problems.push(format!("job #{id} has been in {stage} for {} min", secs / 60));
    }
    if problems.is_empty() {
        Check::ok(counts)
    } else {
        Check::degraded(format!("{}; {counts}", problems.join("; ")))
    }
}

/// Host component of a lease owner string.
///
/// Recovery only requeues jobs whose owner starts with this host, so two
/// daemons sharing a database never steal each other's work.
fn hostname_for_lease() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// The ffmpeg the daemon runs: `config.json`'s path, else `bin/`'s. Every
/// yt-dlp run is handed the same one for its merge (P11.4e).
pub(crate) fn resolve_ffmpeg(config: &AppConfig, dep_mgr: &DependencyManager) -> PathBuf {
    config
        .ffmpeg_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dep_mgr.find_binary("ffmpeg"))
        .unwrap_or_else(|| dep_mgr.unresolved_path("ffmpeg"))
}

/// One nightly yt-dlp update: verify, wait for the pool, swap (plan P2.9).
///
/// Every step can decline without consequence. A checksum mismatch, a pool that
/// will not drain, a failed rename — each leaves the working binary in place
/// and tries again the following night. Late is fine; a half-installed
/// downloader in the middle of a news cycle is not.
async fn nightly_ytdl_update(
    dep_mgr: &DependencyManager,
    channel: &str,
    gate: &UpdateGate,
    repo: &Repository,
    pacer: &omni_broadcast::pacing::HostPacer,
    ffmpeg: Option<&std::path::Path>,
) -> omni_core::scheduler::TaskOutcome {
    use omni_core::scheduler::TaskOutcome;

    info!("Running scheduled nightly yt-dlp update check...");

    // The one swapper at a time (the admin's Install, the legacy routes, Deno): staging
    // shares `yt-dlp.staged`, and the gate has one flag.
    let mut swap = gate.lock_swaps().await;
    let staged = match dep_mgr.stage_ytdl(channel).await {
        Ok(s) => s,
        Err(e) => {
            // Includes a checksum mismatch, which is the case the old code
            // would have installed silently.
            warn!("Nightly yt-dlp update not installed: {e:#}");
            let _ = repo.log_audit("WARN", "SYSTEM", &format!("yt-dlp update skipped: {e}"));
            return TaskOutcome::Failed;
        }
    };

    // A build the self-check already rolled back is not tried again every
    // night; the next release is.
    let rejected_marker = dep_mgr.get_bin_dir().join("yt-dlp.exe.rejected");
    let rejected = std::fs::read_to_string(&rejected_marker).unwrap_or_default();
    if rejected.trim() == staged.sha256 {
        dep_mgr.discard_staged_ytdl();
        info!("yt-dlp update skipped: this build was rolled back before");
        return TaskOutcome::Skipped;
    }
    // Live and also the release: nothing to do. (The download is still made,
    // to know; it is a few seconds at 03:00.)
    if dep_mgr.ytdl_live_sha256().as_deref() == Some(staged.sha256.as_str()) {
        dep_mgr.discard_staged_ytdl();
        info!("yt-dlp is already the latest release");
        // No update is waiting any more (P11.4).
        selfcheck::note_postponed(&dep_mgr.get_bin_dir(), false);
        return TaskOutcome::Ok;
    }

    // Up to ten minutes for in-flight downloads. Longer than any sane clip and
    // shorter than the gap to the next bulletin.
    if !swap.drain(Duration::from_secs(600)).await {
        warn!(
            "yt-dlp update postponed: {} download(s) still running after 10 minutes. The \
             verified build stays staged and will be applied on the next attempt.",
            swap.active()
        );
        // Skipped, not failed: nothing is wrong, the pipeline was simply busy,
        // and an operator reading the maintenance panel should not be sent
        // looking for a fault that does not exist.
        return TaskOutcome::Skipped;
    }

    let outcome = dep_mgr.apply_staged_ytdl();
    let installed = match outcome {
        Ok(path) => path,
        Err(e) => {
            warn!("Nightly yt-dlp update failed to install: {e:#}");
            let _ = repo.log_audit("ERROR", "SYSTEM", &format!("yt-dlp update failed: {e}"));
            return TaskOutcome::Failed;
        }
    };

    // Is the new build at least as good as the old one (plan P6.7)? The
    // links that worked before the update are checked again, with downloads
    // still paused. A link the new build fails and the old one passes means
    // the release broke it: the old build goes back and this one is marked
    // rejected. A link both fail is the site, not the update.
    let verdict = verify_ytdl_update(dep_mgr, &installed, repo, pacer, ffmpeg).await;
    drop(swap);
    let act = ytdl_verdict_action(&verdict, channel, &staged.sha256[..16]);
    if act.reject {
        let _ = std::fs::write(&rejected_marker, &staged.sha256);
    }
    if act.level == "INFO" {
        info!("Nightly yt-dlp update: {}", act.audit);
    } else {
        warn!("Nightly yt-dlp update: {}", act.audit);
    }
    let _ = repo.log_audit(act.level, "SYSTEM", &act.audit);
    if act.close_breakers {
        // A new downloader may resolve what the model once could not be asked about.
        if let Err(e) = repo.close_all_llm_breakers() {
            warn!("The repair model breakers could not be closed after the update: {e:#}");
        }
    }
    if let Some(postponed) = act.postponed_night {
        selfcheck::note_postponed(&dep_mgr.get_bin_dir(), postponed);
    }
    act.outcome
}

/// What the nightly yt-dlp task does with a verdict (P11.4).
#[derive(Debug, PartialEq)]
struct VerdictAction {
    /// Write `yt-dlp.exe.rejected`: this build is never tried again.
    reject: bool,
    /// The audit row's level and text.
    level: &'static str,
    audit: String,
    outcome: omni_core::scheduler::TaskOutcome,
    /// A new downloader is live: the repair model's breakers close.
    close_breakers: bool,
    /// One more postponed night (`Some(true)`), the run of them ends
    /// (`Some(false)`), or unchanged (`None`).
    postponed_night: Option<bool>,
}

/// [`VerdictAction`] for `verdict` on a build of `channel` whose hash begins `sha16`.
fn ytdl_verdict_action(verdict: &UpdateVerdict, channel: &str, sha16: &str) -> VerdictAction {
    use omni_core::scheduler::TaskOutcome;
    match verdict {
        UpdateVerdict::Kept { checked } => VerdictAction {
            reject: false,
            level: "INFO",
            audit: format!(
                "yt-dlp updated ({channel} channel, sha256 {sha16}); {checked} self-check link(s) still work; previous build kept for rollback"
            ),
            outcome: TaskOutcome::Ok,
            close_breakers: true,
            postponed_night: Some(false),
        },
        UpdateVerdict::RolledBack { broken } => VerdictAction {
            reject: true,
            level: "ERROR",
            audit: format!(
                "yt-dlp update rolled back automatically: the new build could not get {} while the previous one could. The previous build is in use; the next release will be tried.",
                broken.join(", ")
            ),
            outcome: TaskOutcome::Failed,
            close_breakers: false,
            postponed_night: Some(false),
        },
        // Not judged: the previous build is live, this one is not marked
        // rejected and is tried again the next night.
        UpdateVerdict::Postponed { why } => VerdictAction {
            reject: false,
            level: "WARN",
            audit: format!("yt-dlp update ({channel} channel, sha256 {sha16}) postponed to the next night: {why}. The previous build is in use."),
            outcome: TaskOutcome::Skipped,
            close_breakers: false,
            postponed_night: Some(true),
        },
        // The new build is live but nobody could tell whether it works.
        UpdateVerdict::Unverified { why } => VerdictAction {
            reject: false,
            level: "WARN",
            audit: format!(
                "yt-dlp ({channel} channel, sha256 {sha16}) is live but could not be verified: {why}. Check Administration → Download engine and roll back if downloads fail."
            ),
            outcome: TaskOutcome::Failed,
            close_breakers: false,
            postponed_night: None,
        },
    }
}

/// Report Deno's state, and install it when it is missing and automatic
/// updates are on. Returns whether a working Deno is in place.
/// The banner's Deno line (P6.8b): yt-dlp needs Deno for YouTube, and a banner
/// without it made a present `bin/deno.exe` look missing. `true` = found.
fn deno_banner_line(dep_mgr: &DependencyManager) -> (bool, String) {
    match dep_mgr.deno_version() {
        Some(v) => (true, format!("  deno:         {:?} ({v})", dep_mgr.deno_live())),
        None => (false, format!("  deno:         missing from {:?}: YouTube downloads fail", dep_mgr.get_bin_dir())),
    }
}

async fn ensure_deno(dep_mgr: &DependencyManager, health: &HealthState, auto: bool) -> bool {
    use omni_core::health::{checks, Check};
    if let Some(v) = dep_mgr.deno_version() {
        health.set(checks::DENO, Check::ok(format!("Deno {v}")));
        return true;
    }
    if !auto {
        health.set(
            checks::DENO,
            Check::degraded(
                "deno.exe is missing from bin/: YouTube downloads fail. Automatic updates are off; put deno.exe in bin/ or turn them on",
            ),
        );
        return false;
    }
    health.set(checks::DENO, Check::degraded("deno.exe is missing: downloading it now; YouTube downloads may fail until it is in place"));
    match dep_mgr.install_deno().await {
        Ok(v) => {
            info!("Deno {v} installed for yt-dlp's YouTube support");
            health.set(checks::DENO, Check::ok(format!("Deno {v}")));
            true
        }
        Err(e) => {
            warn!("Deno could not be installed: {e:#}");
            health.set(
                checks::DENO,
                Check::degraded(format!("deno.exe is missing and could not be downloaded ({e:#}): YouTube downloads fail; it is tried again tonight")),
            );
            false
        }
    }
}

/// The nightly Deno step (plan P6.8): install it if missing; take a newer
/// release only if every self-check link that worked still works, else put
/// the previous one back.
async fn nightly_deno_update(
    dep_mgr: &DependencyManager,
    gate: &UpdateGate,
    repo: &Repository,
    health: &HealthState,
    pacer: &omni_broadcast::pacing::HostPacer,
    ffmpeg: Option<&std::path::Path>,
) -> omni_core::scheduler::TaskOutcome {
    use omni_core::scheduler::TaskOutcome;
    let Some(installed) = dep_mgr.deno_version() else {
        let _swap = gate.lock_swaps().await;
        return if ensure_deno(dep_mgr, health, true).await { TaskOutcome::Ok } else { TaskOutcome::Failed };
    };
    let latest = match dep_mgr.latest_deno_tag().await {
        Ok(t) => t,
        Err(e) => {
            warn!("Deno update check failed: {e:#}");
            return TaskOutcome::Skipped;
        }
    };
    if latest.trim_start_matches('v') == installed {
        return TaskOutcome::Ok;
    }
    // Download first; pause only around the rename.
    let mut swap = gate.lock_swaps().await;
    let new_version = match dep_mgr.stage_deno().await {
        Ok(v) => v,
        Err(e) => {
            warn!("Deno update not installed: {e:#}");
            let _ = repo.log_audit("WARN", "SYSTEM", &format!("Deno update skipped: {e}"));
            return TaskOutcome::Failed;
        }
    };
    if !swap.drain(Duration::from_secs(600)).await {
        dep_mgr.discard_staged_deno();
        return TaskOutcome::Skipped;
    }
    let result = dep_mgr.apply_staged_deno(&new_version);
    let outcome = match result {
        Err(e) => {
            dep_mgr.discard_staged_deno();
            warn!("Deno update not installed: {e:#}");
            let _ = repo.log_audit("WARN", "SYSTEM", &format!("Deno update skipped: {e}"));
            TaskOutcome::Failed
        }
        Ok(()) => {
            let worked: Vec<omni_core::models::SelfcheckLink> = repo
                .list_selfcheck_links()
                .unwrap_or_default()
                .into_iter()
                .filter(|l| l.last_ok == Some(true))
                .collect();
            let ytdl = dep_mgr.ytdl_live();
            let results = selfcheck::check_and_record(repo, &ytdl, ffmpeg, pacer, &worked).await;
            // A rollback that fails leaves the new Deno live and unverified (P11.4).
            let verdict = match selfcheck::judge_deno(&results) {
                UpdateVerdict::RolledBack { broken } if dep_mgr.rollback_deno().is_err() => UpdateVerdict::Unverified {
                    why: format!("{} stopped working with it, and Deno {installed} could not be put back", broken.join(", ")),
                },
                UpdateVerdict::Postponed { why } if dep_mgr.rollback_deno().is_err() => {
                    UpdateVerdict::Unverified { why: format!("{why}, and Deno {installed} could not be put back") }
                }
                v => v,
            };
            match verdict {
                UpdateVerdict::Kept { .. } => {
                    let _ = repo.log_audit("INFO", "SYSTEM", &format!("Deno updated {installed} → {new_version}"));
                    TaskOutcome::Ok
                }
                UpdateVerdict::RolledBack { broken } => {
                    let _ = repo.log_audit(
                        "ERROR",
                        "SYSTEM",
                        &format!("Deno {new_version} rolled back to {installed}: {} stopped working with it", broken.join(", ")),
                    );
                    TaskOutcome::Failed
                }
                UpdateVerdict::Postponed { why } => {
                    // Not judged (P11.4): the previous Deno is back, and the new one is tried again the next night.
                    warn!("Deno update postponed: {why}");
                    let _ = repo.log_audit("WARN", "SYSTEM", &format!("Deno {new_version} postponed to the next night: {why}. Deno {installed} is in use."));
                    TaskOutcome::Skipped
                }
                UpdateVerdict::Unverified { why } => {
                    warn!("Deno {new_version} is live but could not be verified: {why}");
                    let _ = repo.log_audit("WARN", "SYSTEM", &format!("Deno {new_version} is live but could not be verified: {why}"));
                    TaskOutcome::Failed
                }
            }
        }
    };
    drop(swap);
    if let Some(v) = dep_mgr.deno_version() {
        health.set(omni_core::health::checks::DENO, omni_core::health::Check::ok(format!("Deno {v}")));
    }
    outcome
}

/// The new yt-dlp against the links that worked before it ([`selfcheck::verify_update`]):
/// each round is a self-check of those links, and the swap is `rollback_ytdl`
/// (it puts whichever build is not live back).
async fn verify_ytdl_update(
    dep_mgr: &DependencyManager,
    live: &std::path::Path,
    repo: &Repository,
    pacer: &omni_broadcast::pacing::HostPacer,
    ffmpeg: Option<&std::path::Path>,
) -> UpdateVerdict {
    let worked: Vec<omni_core::models::SelfcheckLink> = repo
        .list_selfcheck_links()
        .unwrap_or_default()
        .into_iter()
        .filter(|l| l.last_ok == Some(true))
        .collect();
    selfcheck::verify_update(
        &worked,
        |links| async move { selfcheck::check_and_record(repo, live, ffmpeg, pacer, &links).await },
        || match dep_mgr.rollback_ytdl() {
            Ok(_) => true,
            Err(e) => {
                warn!("yt-dlp verification: could not swap the builds: {e:#}");
                false
            }
        },
        |links, why| selfcheck::withdraw_results(repo, links, why),
    )
    .await
}

/// Replace a one-file yt-dlp with the folder build of the *same release*
/// (plan P2.10): same extractors, ~3-17 s less per yt-dlp start. A newer
/// release still only arrives through the nightly update and its
/// self-check. Any failure leaves the one-file exe in use.
async fn ensure_ytdl_folder_build(dep_mgr: &DependencyManager, channel: &str, auto: bool) {
    if !auto || dep_mgr.ytdl_is_folder_build() || !dep_mgr.ytdl_legacy().is_file() {
        return;
    }
    let legacy = dep_mgr.ytdl_legacy();
    let version = match omni_core::process::run_capture(&legacy, ["--version"], Duration::from_secs(60)).await {
        Ok(o) if o.success => o.stdout.lines().next().unwrap_or("").trim().to_string(),
        _ => String::new(),
    };
    if version.is_empty() || !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        warn!("yt-dlp folder build not installed: could not read the version of {legacy:?}");
        return;
    }
    info!("Installing the folder build of yt-dlp {version} (starts in ~1 s instead of 4-18 s)");
    let staged = tokio::time::timeout(Duration::from_secs(120), dep_mgr.stage_ytdl_release(channel, Some(&version))).await;
    match staged {
        Ok(Ok(_)) => match dep_mgr.apply_staged_ytdl() {
            Ok(p) => info!("yt-dlp {version} now runs from {p:?}; {legacy:?} is kept as the fallback"),
            Err(e) => {
                dep_mgr.discard_staged_ytdl();
                warn!("yt-dlp folder build not installed: {e:#}");
            }
        },
        Ok(Err(e)) => warn!("yt-dlp folder build not installed: {e:#}"),
        Err(_) => {
            dep_mgr.discard_staged_ytdl();
            warn!("yt-dlp folder build not installed: download took over 2 minutes");
        }
    }
}

/// Everything the maintenance tasks need, gathered once at start-up.
/// Restrict `dirs` to SYSTEM, Administrators and the service account (P3.5).
/// Does nothing unless `service_mode`. A failure is a warning and an amber
/// health line; it never stops the service.
async fn lock_down_install(service_mode: bool, dirs: &[PathBuf], health: &HealthState) {
    use omni_core::health::{checks, Check};
    if !service_mode {
        return;
    }
    // LocalSystem re-applies the restriction. A named service account has Modify, which
    // cannot change an ACL, so there the start only checks what install set.
    use omni_core::lockdown::{action_for, Action};
    let sid = omni_core::lockdown::current_sid().await;
    let result = match action_for(sid.as_deref()) {
        Action::Apply => omni_core::lockdown::apply(dirs, omni_core::lockdown::Account::System).await,
        Action::Verify => omni_core::lockdown::verify(dirs).await,
    };
    match result {
        Ok(()) => health.set(checks::LOCKDOWN, Check::ok("data and temp folders are restricted")),
        Err(e) => {
            warn!("Folder permissions not restricted: {e:#}");
            health.set(checks::LOCKDOWN, Check::degraded(format!("data and temp folders are not restricted: {e:#}")));
        }
    }
}

/// The Cookies card's «Test» (P3.5): yt-dlp with the site's jar on a sample link.
struct CookieJarTester {
    repo: Repository,
    bin_dir: PathBuf,
    temp_path: PathBuf,
    /// The daemon's pacer (P11.4): the test takes its turn at the platform like everything else.
    pacer: omni_broadcast::pacing::HostPacer,
    /// The daemon's ffmpeg, handed to yt-dlp (P11.4e).
    ffmpeg: Option<PathBuf>,
}

impl omni_web::state::CookieTester for CookieJarTester {
    fn test<'a>(&'a self, domain: &'a str, url: &'a str) -> omni_web::state::CookieTestFuture<'a> {
        Box::pin(async move {
            let ytdl = DependencyManager::new(&self.bin_dir).ytdl_live();
            let t = omni_broadcast::cookie_file::test_jar(&ytdl, self.ffmpeg.as_deref(), &self.temp_path, &self.repo, domain, url, &self.pacer).await?;
            // The site was limiting requests: nothing was learnt or stored; the admin is told to try later.
            if t.inconclusive {
                return Err(t.detail);
            }
            Ok((t.ok, t.detail))
        })
    }
}

struct MaintenanceContext {
    bin_dir: PathBuf,
    temp_path: PathBuf,
    paths: AppPaths,
    ytdl_channel: String,
    ytdl_enabled: bool,
    adblock_enabled: bool,
    gate: UpdateGate,
    retention_days: i64,
    archive: Option<omni_core::archive::ArchiveSettings>,
    mail_text_retention_days: i64,
    evidence_days: i64,
    evidence_max_bytes: u64,
    health: HealthState,
    canary: Arc<canary::CanaryEnv>,
}

/// Run whatever is due, recording the outcome of each (plan P6.6).
async fn run_due_tasks(
    repo: &Repository,
    specs: &[omni_core::scheduler::TaskSpec],
    ctx: &MaintenanceContext,
) {
    use omni_core::scheduler::TaskOutcome;

    let due = match repo.due_tasks() {
        Ok(d) => d,
        Err(e) => {
            error!("Could not read the maintenance schedule: {e:?}");
            return;
        }
    };

    for name in due {
        let Some(spec) = specs.iter().find(|s| s.name == name) else {
            // A row for a task this build no longer has. Left alone rather
            // than deleted: a downgrade should not lose its history.
            continue;
        };

        // Claim before working. `next_run` moves forward first, so a task that
        // outlasts the tick interval is not started again on the next tick.
        let next = spec.cadence.next_after(chrono::Utc::now());
        match repo.claim_task(&name, next) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                error!("Could not claim maintenance task {name}: {e:?}");
                continue;
            }
        }

        // Jitter applies to the tasks that hit a shared external service, so a
        // fleet of installs does not arrive at the same endpoint together.
        let delay = omni_core::scheduler::jitter(spec.jitter);
        if !delay.is_zero() {
            info!("Maintenance [{name}] starting in {}s (jitter)", delay.as_secs());
            tokio::time::sleep(delay).await;
        }

        info!("Maintenance [{name}] running: {}", spec.description);
        let started = std::time::Instant::now();
        let result = run_one_task(&name, repo, ctx).await;
        let elapsed_ms = started.elapsed().as_millis() as i64;

        match &result {
            Ok(TaskOutcome::Ok) => info!("Maintenance [{name}] ok in {elapsed_ms} ms"),
            Ok(TaskOutcome::Skipped) => info!("Maintenance [{name}] skipped"),
            Ok(TaskOutcome::Failed) | Err(_) => {}
        }

        let (outcome, error) = match result {
            Ok(o) => (o, None),
            Err(e) => {
                warn!("Maintenance [{name}] failed: {e:#}");
                (TaskOutcome::Failed, Some(format!("{e:#}")))
            }
        };
        let _ = repo.record_task_run(&name, outcome, error.as_deref(), elapsed_ms);
    }
}

async fn run_one_task(
    name: &str,
    repo: &Repository,
    ctx: &MaintenanceContext,
) -> Result<omni_core::scheduler::TaskOutcome> {
    use omni_core::scheduler::TaskOutcome;

    match name {
        "ytdl_update" => {
            if !ctx.ytdl_enabled {
                return Ok(TaskOutcome::Skipped);
            }
            let dep_mgr = DependencyManager::new(&ctx.bin_dir);
            // No canary run (its yt-dlp starts without a gate pass) overlaps the swap.
            let (ytdl, deno) = canary::with_canary_lock(&ctx.canary, async {
                let ffmpeg = ctx.canary.ffmpeg();
                let ytdl = nightly_ytdl_update(&dep_mgr, &ctx.ytdl_channel, &ctx.gate, repo, &ctx.canary.pacer(), ffmpeg.as_deref()).await;
                let deno = nightly_deno_update(&dep_mgr, &ctx.gate, repo, &ctx.health, &ctx.canary.pacer(), ffmpeg.as_deref()).await;
                (ytdl, deno)
            })
            .await;
            selfcheck::refresh_health(repo, &ctx.health, &ctx.bin_dir);
            // The worse of the two is what the maintenance panel shows.
            Ok(match (ytdl, deno) {
                (TaskOutcome::Failed, _) | (_, TaskOutcome::Failed) => TaskOutcome::Failed,
                (TaskOutcome::Ok, _) | (_, TaskOutcome::Ok) => TaskOutcome::Ok,
                _ => TaskOutcome::Skipped,
            })
        }

        "selfcheck" => {
            let ytdl = DependencyManager::new(&ctx.bin_dir).ytdl_live();
            let failing = selfcheck::run(repo, &ytdl, ctx.canary.ffmpeg().as_deref(), &ctx.canary.pacer(), &ctx.health, &ctx.bin_dir).await?;
            // Then the recent articles of each frequent site (P10.4). Always
            // on; a site's verdict never changes this task's outcome.
            canary::nightly(&ctx.canary).await;
            // Then, for every site still broken, the repair ladder on its canary
            // pages, probe only (P10.14).
            canary::heal_nightly(&ctx.canary).await;
            if failing > 0 {
                anyhow::bail!("{failing} self-check link(s) not working; see Administration → Self-check");
            }
            Ok(TaskOutcome::Ok)
        }

        "cookie_test" => {
            // Every jar against the last link it was tested with (P3.5), so an
            // expired login is seen overnight and not by a journalist.
            let jars = repo.list_cookie_jars()?;
            let ytdl = DependencyManager::new(&ctx.bin_dir).ytdl_live();
            let ffmpeg = ctx.canary.ffmpeg();
            let (mut tested, mut failing) = (0usize, Vec::<String>::new());
            for jar in &jars {
                let Some(url) = omni_core::cookies::sample_url_for(&jar.domain, jar.last_sample_url.as_deref()) else { continue };
                tested += 1;
                match omni_broadcast::cookie_file::test_jar(&ytdl, ffmpeg.as_deref(), &ctx.temp_path, repo, &jar.domain, &url, &ctx.canary.pacer()).await {
                    Ok(t) if t.ok => {}
                    // The site was limiting requests: not a failed jar, and nothing was stored.
                    Ok(t) if t.inconclusive => info!("Cookie test for {}: not judged tonight, {}", jar.domain, t.detail),
                    Ok(_) => failing.push(jar.domain.clone()),
                    Err(why) => {
                        warn!("Cookie test for {} could not run: {why}", jar.domain);
                        failing.push(jar.domain.clone());
                    }
                }
            }
            omni_core::cookies::refresh_health(repo, &ctx.health);
            if tested == 0 {
                return Ok(TaskOutcome::Skipped);
            }
            if !failing.is_empty() {
                anyhow::bail!("The cookies for {} no longer log in; see Administration → Cookies", failing.join(", "));
            }
            Ok(TaskOutcome::Ok)
        }

        "adblock_update" => {
            if !ctx.adblock_enabled {
                return Ok(TaskOutcome::Skipped);
            }
            let blocker = omni_browser::UnifiedAdBlocker::global();
            let stats = blocker
                .update_blocklists()
                .await
                .context("Blocklist update failed")?;
            info!("Blocklists updated ({} active domains)", stats.total_domains);
            Ok(TaskOutcome::Ok)
        }

        "retention" => {
            // Login records first: the table grows with every failed attempt
            // and nothing else prunes it.
            match canary::sync_pool(repo) {
                Ok((added, removed)) if added + removed > 0 => {
                    info!("Retention: site canaries +{added} -{removed}")
                }
                Ok(_) => {}
                Err(e) => warn!("Retention: site canaries not refreshed: {e:#}"),
            }
            match repo.purge_idle_sites(90) {
                Ok(n) if n > 0 => {
                    info!("Retention: removed {n} idle site record(s)");
                    // A purged degraded site must leave the banner too.
                    omni_core::site_health::refresh_health(repo, &ctx.health);
                }
                Ok(_) => {}
                Err(e) => warn!("Retention: site records not purged: {e:#}"),
            }
            // Learned recipes (P10.10): idle and failing ones retire, old ones go,
            // and the caps hold.
            match repo.recipe_gc(chrono::Utc::now()) {
                Ok(rep) if rep.changed() => info!("Retention: recipe store {rep:?}"),
                Ok(_) => {}
                Err(e) => warn!("Retention: recipe store not cleaned: {e:#}"),
            }
            match repo.purge_repair_attempts(90) {
                Ok(n) if n > 0 => info!("Retention: removed {n} old repair attempt(s)"),
                Ok(_) => {}
                Err(e) => warn!("Retention: repair attempts not purged: {e:#}"),
            }
            // Wrong videos MCR marked (P10.13) are forgotten after their 30 days.
            match repo.purge_repair_skips(chrono::Utc::now()) {
                Ok(n) if n > 0 => info!("Retention: removed {n} expired repair skip(s)"),
                Ok(_) => {}
                Err(e) => warn!("Retention: repair skips not purged: {e:#}"),
            }
            // The overnight self-heal's reports (P10.14): 30 days (and at most 500 rows, on write).
            match repo.purge_selfheal(chrono::Utc::now()) {
                Ok(n) if n > 0 => info!("Retention: removed {n} old self-heal report(s)"),
                Ok(_) => {}
                Err(e) => warn!("Retention: self-heal reports not purged: {e:#}"),
            }
            // The model rung's limits (P10.12): a bundle is remembered 24 h, a day's totals 90 days.
            {
                let now = chrono::Utc::now();
                let usage_before = (chrono::Local::now() - chrono::Duration::days(90)).format("%Y-%m-%d").to_string();
                match repo.purge_llm_limits(now - chrono::Duration::hours(24), &usage_before) {
                    Ok(n) if n > 0 => info!("Retention: removed {n} old repair model record(s)"),
                    Ok(_) => {}
                    Err(e) => warn!("Retention: repair model records not purged: {e:#}"),
                }
            }
            match repo.purge_login_attempts(90) {
                Ok(n) if n > 0 => info!("Retention: removed {n} old login record(s)"),
                Ok(_) => {}
                Err(e) => warn!("Retention: login records not purged: {e:#}"),
            }
            // Which videos the browser saw on which article (P3.16): the week that counts, twice over.
            match repo.purge_site_videos(chrono::Utc::now() - chrono::Duration::days(omni_core::site_videos::SITE_VIDEOS_KEEP_DAYS)) {
                Ok(n) if n > 0 => info!("Retention: removed {n} old site video sighting(s)"),
                Ok(_) => {}
                Err(e) => warn!("Retention: site video sightings not purged: {e:#}"),
            }

            // Finished jobs, audit entries, handled-mail records and expired sessions
            // (P1.21): 90 / 365 / 90 days, in batches.
            match repo.purge_old_records(chrono::Utc::now()) {
                Ok(c) => {
                    if c.total() > 0 {
                        info!(
                            "Retention: removed {} job(s), {} timeline row(s), {} evidence record(s), {} audit entr(ies), {} mail record(s), {} expired session(s), {} outcome row(s)",
                            c.jobs, c.job_events, c.evidence, c.audit, c.mail, c.sessions, c.outcomes
                        );
                    }
                    for e in &c.errors {
                        warn!("Retention: old records not fully purged: {e}");
                    }
                }
                Err(e) => warn!("Retention: old records not purged: {e:#}"),
            }

            // The text of handled mail (plan P7.6): the MCR mail view needs it
            // for a few weeks, nobody needs it for ever. Sender, subject and
            // jobs stay with the row.
            match repo.purge_mail_text(ctx.mail_text_retention_days) {
                Ok(n) if n > 0 => info!("Retention: cleared the text of {n} handled mail(s)"),
                Ok(_) => {}
                Err(e) => warn!("Retention: mail text not cleared: {e:#}"),
            }

            // Orphaned per-job workspaces. The start-up sweep only runs at
            // start-up, and a machine that stays up for a month accumulates
            // the temp directories of every job that died mid-stage.
            let removed = sweep_orphan_job_dirs(repo, &ctx.temp_path.join("jobs"), ctx.retention_days).await;
            if removed > 0 {
                info!("Retention: removed {removed} orphaned job workspace(s)");
            }
            // Email attachments (plan P4.6) live outside temp/jobs so the
            // start-up sweep cannot take a queued job's only source; this is
            // where they go once their job is long finished.
            let removed = sweep_orphan_job_dirs(repo, &ctx.temp_path.join("attachments"), ctx.retention_days).await;
            if removed > 0 {
                info!("Retention: removed {removed} saved email attachment(s)");
            }
            // Delivered sources past their days (P1.7), by the date in the
            // directory name.
            if let Some(a) = ctx.archive.clone().filter(|a| a.purges()) {
                let today = chrono::Local::now().date_naive();
                match tokio::task::spawn_blocking(move || omni_core::archive::purge_expired(&a.root, today, a.retention_days)).await {
                    Ok(n) if n > 0 => info!("Retention: removed {n} expired archive day(s)"),
                    Ok(_) => {}
                    Err(e) => warn!("Retention: archive not purged: {e}"),
                }
            }
            sweep_evidence(repo, &ctx.temp_path.join("evidence"), ctx.evidence_days, ctx.evidence_max_bytes);
            // A benchmark killed mid-run leaves cookie copies behind; nothing running, nothing kept.
            benchmark::sweep_idle_scratch(repo, &ctx.paths);
            Ok(TaskOutcome::Ok)
        }

        "vacuum" => {
            // Blocking, and it holds a write lock, which is why it is monthly
            // and at 04:30 rather than opportunistic.
            repo.vacuum_database().context("VACUUM failed")?;
            Ok(TaskOutcome::Ok)
        }

        other => {
            warn!("Maintenance: no handler for task {other}");
            Ok(TaskOutcome::Skipped)
        }
    }
}

/// What the worker loop leases now. With the `disk` check red (`disk_ok`
/// false) only a video that waits for the watchfolder (P11.2a): it needs no
/// temp space and delivering it frees some; ordinary jobs and approved held
/// videos wait for room as before. Otherwise any job with a download slot,
/// and without one only a deliver-only job (approved held, or waiting). A
/// waiting video only while the watchfolder answers (`deliver_ok`).
fn lease_now(
    repo: &Repository,
    owner: &str,
    disk_ok: bool,
    has_download_slot: bool,
    deliver_ok: bool,
) -> anyhow::Result<Option<omni_core::models::Job>> {
    if !disk_ok {
        repo.lease_waiting_delivery_when(owner, LEASE_SECS, deliver_ok)
    } else if has_download_slot {
        repo.lease_job_when(owner, LEASE_SECS, deliver_ok)
    } else {
        repo.lease_approved_held_job_when(owner, LEASE_SECS, deliver_ok)
    }
}

/// The start-up sweep of `temp/jobs`: every job's workspace goes but those of
/// RUNNING jobs and of jobs whose finished video waits for the watchfolder
/// (P11.2a). Runs after start-up recovery, which turns a waiting job that was
/// RUNNING when the daemon stopped back into a waiting one.
async fn startup_sweep(repo: &Repository, jobs_root: &std::path::Path) -> anyhow::Result<usize> {
    let mut keep = repo.running_job_ids()?;
    keep.extend(repo.parked_job_ids()?);
    Ok(omni_broadcast::delivery::WatchfolderDelivery::sweep_orphan_job_dirs(jobs_root, &keep).await)
}

/// Remove `{dir}/{id}` directories (`temp/jobs`, `temp/attachments`) whose
/// job is gone or long finished.
///
/// Deliberately keyed on the directory name being a job id, never on a
/// filename prefix: job 1's prefix also matches jobs 10-19 and 100-199, which
/// is defect D-04 and is exactly why per-job directories exist.
async fn sweep_orphan_job_dirs(repo: &Repository, jobs_dir: &std::path::Path, keep_days: i64) -> usize {
    let Ok(entries) = std::fs::read_dir(jobs_dir) else {
        return 0;
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::days(keep_days.max(1));
    let mut removed = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<i64>().ok())
        else {
            continue;
        };

        let stale = match repo.get_job(id) {
            // No such job: nothing will ever come back for these files.
            Ok(None) => true,
            // Finished long enough ago that a retry is not coming.
            Ok(Some(job)) => {
                job.status.is_terminal()
                    && job.updated_at.map(|t| t < cutoff).unwrap_or(false)
            }
            Err(_) => false,
        };

        if stale && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Which evidence rows retention deletes (P10.1): older than `keep_days`, or
/// whose job is gone; then the oldest until the rest fit `max_bytes`. A
/// RUNNING job's row is never chosen. `rows` come oldest first.
fn evidence_to_delete(
    rows: &[omni_core::repository::EvidenceRow],
    now: chrono::DateTime<chrono::Utc>,
    keep_days: i64,
    max_bytes: u64,
    is_running: impl Fn(i64) -> bool,
    job_exists: impl Fn(i64) -> bool,
) -> Vec<i64> {
    let cutoff = now - chrono::Duration::days(keep_days.max(1));
    let mut chosen = vec![false; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        if is_running(r.job_id) {
            continue;
        }
        chosen[i] = !job_exists(r.job_id) || r.created_at.is_some_and(|t| t < cutoff);
    }
    let mut total: u64 = rows
        .iter()
        .zip(&chosen)
        .filter(|(_, c)| !**c)
        .map(|(r, _)| r.bytes.max(0) as u64)
        .sum();
    for (i, r) in rows.iter().enumerate() {
        if total <= max_bytes {
            break;
        }
        if chosen[i] || is_running(r.job_id) {
            continue;
        }
        chosen[i] = true;
        total -= r.bytes.max(0) as u64;
    }
    rows.iter().zip(chosen).filter(|(_, c)| *c).map(|(r, _)| r.job_id).collect()
}

/// What retention knows about a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobLookup {
    Missing,
    Running,
    Present,
}

/// Retention for `temp/evidence`: rows chosen by [`evidence_to_delete`], and
/// `{id}.json` files that have no row. Each file is matched by its whole name,
/// never by prefix.
fn sweep_evidence(repo: &Repository, dir: &std::path::Path, keep_days: i64, max_bytes: u64) {
    let rows = match repo.list_evidence() {
        Ok(r) => r,
        Err(e) => return warn!("Retention: evidence list not read: {e:#}"),
    };
    // A failed lookup keeps the evidence: treated as a running job.
    let lookup = |id: i64| match repo.get_job(id) {
        Ok(None) => JobLookup::Missing,
        Ok(Some(j)) if j.status == JobStatus::Running => JobLookup::Running,
        Ok(Some(_)) => JobLookup::Present,
        Err(_) => JobLookup::Running,
    };
    let (removed, stray) = sweep_evidence_with(
        &rows,
        dir,
        chrono::Utc::now(),
        keep_days,
        max_bytes,
        &lookup,
        &|id| repo.delete_evidence(id).is_ok(),
    );
    if removed > 0 || stray > 0 {
        info!("Retention: removed {removed} evidence file(s) and {stray} without a record");
    }
}

/// The sweep itself, with the job lookup and the row delete injected.
/// Returns (files with a row removed, stray files removed).
fn sweep_evidence_with(
    rows: &[omni_core::repository::EvidenceRow],
    dir: &std::path::Path,
    now: chrono::DateTime<chrono::Utc>,
    keep_days: i64,
    max_bytes: u64,
    lookup: &dyn Fn(i64) -> JobLookup,
    delete_row: &dyn Fn(i64) -> bool,
) -> (usize, usize) {
    let doomed = evidence_to_delete(
        rows,
        now,
        keep_days,
        max_bytes,
        |id| lookup(id) == JobLookup::Running,
        |id| lookup(id) != JobLookup::Missing,
    );
    let mut removed = 0;
    for id in &doomed {
        let Some(row) = rows.iter().find(|r| r.job_id == *id) else { continue };
        match std::fs::remove_file(&row.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!("Retention: evidence file of job #{id} not removed: {e}");
                continue;
            }
        }
        if delete_row(*id) {
            removed += 1;
        }
    }
    // Files whose row is gone (a crash between the write and the insert).
    // Not a running job's, and not one so fresh its row may be about to appear.
    let known: std::collections::HashSet<i64> = rows.iter().map(|r| r.job_id).collect();
    let mut stray = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|n| n.parse::<i64>().ok())
            else {
                continue;
            };
            if !path.is_file() || known.contains(&id) || lookup(id) == JobLookup::Running {
                continue;
            }
            let old_enough = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= Duration::from_secs(3600));
            if old_enough && std::fs::remove_file(&path).is_ok() {
                stray += 1;
            }
        }
    }
    (removed, stray)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_banner_names_deno_even_when_it_is_missing() {
        // An empty bin dir that need not exist: nothing is written.
        let dir = std::env::temp_dir().join(format!("omni-p68b-{}", std::process::id()));
        let (found, line) = super::deno_banner_line(&omni_core::dependencies::DependencyManager::new(&dir));
        assert!(!found);
        assert!(line.contains("deno:") && line.contains("missing"), "{line}");
    }

    #[tokio::test]
    async fn console_mode_never_restricts_the_folders() {
        use omni_core::health::checks;
        let dir = std::env::temp_dir();
        let health = HealthState::new();
        lock_down_install(false, &[dir], &health).await;
        assert!(health.get(checks::LOCKDOWN).is_none(), "a console run must not touch permissions or report on it");
    }

    use super::*;
    use omni_core::health::Health;
    use omni_core::models::QueueWatch;

    /// P10.5: the worker holding the lease parks a held job; only it can.
    #[cfg(debug_assertions)]
    #[test]
    fn dev_hold_refuses_the_live_install() {
        let paths = AppPaths::with_root("C:/omni-install", "config.json");
        let live_db = paths.root.join("data").join("omni.db");
        assert!(dev_hold_guard(&paths, false, &paths.root.join("dev").join("omni.db")).is_err(), "no explicit --config");
        assert!(dev_hold_guard(&paths, true, &live_db).is_err(), "the live database");
        assert!(dev_hold_guard(&paths, true, &paths.root.join("dev").join("omni.db")).is_err(), "config.json of the root");
        let dev = AppPaths::with_root("C:/omni-install", "dev/config.json");
        assert!(dev_hold_guard(&dev, true, &dev.root.join("dev").join("omni.db")).is_ok());
        assert!(dev_hold_guard(&dev, true, std::path::Path::new("C:/OMNI-INSTALL/Data/omni.db")).is_err(), "case-insensitive on Windows paths");
    }

    fn ev_row(job_id: i64, mb: i64, age_days: i64, now: chrono::DateTime<chrono::Utc>) -> omni_core::repository::EvidenceRow {
        omni_core::repository::EvidenceRow {
            job_id,
            path: format!("temp/evidence/{job_id}.json"),
            bytes: mb * 1024 * 1024,
            created_at: Some(now - chrono::Duration::days(age_days)),
        }
    }

    #[test]
    fn evidence_over_the_size_cap_goes_oldest_first() {
        let now = chrono::Utc::now();
        // 30 files of 10 MB = 300 MB, oldest first, all recent.
        let rows: Vec<_> = (1..=30).map(|i| ev_row(i, 10, 30 - i.min(29) / 3, now)).collect();
        let max = 200 * 1024 * 1024;
        let gone = evidence_to_delete(&rows, now, 60, max, |_| false, |_| true);
        let kept: i64 = rows.iter().filter(|r| !gone.contains(&r.job_id)).map(|r| r.bytes).sum();
        assert!(kept as u64 <= max, "kept {kept}");
        assert_eq!(gone, (1..=10).collect::<Vec<_>>(), "the ten oldest, in order");
    }

    #[test]
    fn the_evidence_sweep_touches_only_whole_id_json_names() {
        let dir = std::env::temp_dir().join(format!(
            "omni-evidence-sweep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        let write = |name: &str, aged: bool| {
            let p = dir.join(name);
            std::fs::write(&p, "{}").unwrap();
            if aged {
                std::fs::File::options().write(true).open(&p).unwrap().set_modified(old).unwrap();
            }
        };
        write("7.json", true); // stray, job gone
        write("17.json", true); // has a row, old
        write("abc.json", true);
        write("7.json.bak", true);
        write("8.json", true); // stray, but its job is running
        write("9.json", false); // stray, but written a minute ago
        write("10.json", true); // stray, job exists but is not running

        let now = chrono::Utc::now();
        let mut row = ev_row(17, 1, 20, now);
        row.path = dir.join("17.json").to_string_lossy().into_owned();
        let deleted = std::cell::RefCell::new(Vec::new());
        let lookup = |id: i64| match id {
            8 => JobLookup::Running,
            7 => JobLookup::Missing,
            _ => JobLookup::Present,
        };
        let (removed, stray) = sweep_evidence_with(
            &[row],
            &dir,
            now,
            14,
            200 * 1024 * 1024,
            &lookup,
            &|id| {
                deleted.borrow_mut().push(id);
                true
            },
        );

        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(left, vec!["7.json.bak", "8.json", "9.json", "abc.json"]);
        assert_eq!((removed, stray), (1, 2), "17 by its row; 7 and 10 as strays");
        assert_eq!(*deleted.borrow(), vec![17]);
    }

    #[test]
    fn evidence_of_a_running_job_is_never_chosen() {
        let now = chrono::Utc::now();
        let rows = vec![ev_row(1, 150, 40, now), ev_row(2, 100, 1, now)];
        // Job 1 is oldest, past the age limit, and over the cap, but running.
        let gone = evidence_to_delete(&rows, now, 14, 200 * 1024 * 1024, |id| id == 1, |_| true);
        assert!(!gone.contains(&1));
        assert_eq!(gone, vec![2], "the size cap falls on the next oldest instead");
        let gone = evidence_to_delete(&rows, now, 14, 200 * 1024 * 1024, |id| id == 1, |id| id != 1);
        assert!(!gone.contains(&1), "running wins over a missing job");
    }

    #[test]
    fn evidence_of_a_vanished_job_or_past_the_age_limit_is_chosen() {
        let now = chrono::Utc::now();
        let rows = vec![ev_row(1, 1, 1, now), ev_row(2, 1, 20, now), ev_row(3, 1, 2, now)];
        let gone = evidence_to_delete(&rows, now, 14, 200 * 1024 * 1024, |_| false, |id| id != 3);
        assert_eq!(gone, vec![2, 3]);
        let none = evidence_to_delete(&rows, now, 14, 200 * 1024 * 1024, |_| false, |_| true);
        assert_eq!(none, vec![2], "only the 20-day-old file is past 14 days");
    }

    #[test]
    fn queue_check_is_ok_until_something_is_slow_and_never_down() {
        let ok = queue_check(&QueueWatch {
            waiting: 2,
            running: 1,
            oldest_ready_wait_secs: Some(QUEUE_SLOW_SECS),
            longest_step: Some((3, "DOWNLOAD".into(), 60)),
        }, false);
        assert_eq!(ok.state, Health::Ok);
        assert_eq!(ok.detail.as_deref(), Some("2 waiting, 1 running"));

        let wait = queue_check(&QueueWatch {
            waiting: 1,
            running: 0,
            oldest_ready_wait_secs: Some(42 * 60 + 5),
            longest_step: None,
        }, false);
        assert_eq!(wait.state, Health::Degraded);
        assert!(wait.detail.unwrap().contains("a video has waited 42 min"));

        let step = queue_check(&QueueWatch {
            waiting: 0,
            running: 1,
            oldest_ready_wait_secs: None,
            longest_step: Some((17, "DOWNLOAD".into(), 35 * 60)),
        }, false);
        assert_eq!(step.state, Health::Degraded);
        assert!(step.detail.unwrap().contains("job #17 has been in DOWNLOAD for 35 min"));

        let both = queue_check(&QueueWatch {
            waiting: 1,
            running: 1,
            oldest_ready_wait_secs: Some(42 * 60),
            longest_step: Some((17, "DOWNLOAD".into(), 35 * 60)),
        }, false);
        assert_eq!(both.state, Health::Degraded);
        let d = both.detail.unwrap();
        assert!(d.contains("a video has waited 42 min") && d.contains("job #17 has been in DOWNLOAD for 35 min"));

        // Hysteresis: 27 min stays degraded once degraded, is ok when not; 24 min recovers.
        let near = |mins: i64, was: bool| queue_check(&QueueWatch {
            waiting: 1,
            running: 1,
            oldest_ready_wait_secs: Some(mins * 60),
            longest_step: Some((5, "DOWNLOAD".into(), mins * 60)),
        }, was);
        assert_eq!(near(27, true).state, Health::Degraded);
        assert_eq!(near(27, false).state, Health::Ok);
        assert_eq!(near(24, true).state, Health::Ok);
    }

    /// The code a page's evidence gives a failed job (P10.2): a "no video"
    /// verdict needs the second look; a weak page keeps what it had.
    #[test]
    fn only_four_codes_count_against_a_site() {
        let counting: Vec<&str> = omni_broadcast::errors::ErrorCode::ALL
            .iter()
            .map(|c| c.as_str())
            .filter(|c| omni_core::site_health::counts_against_site(c))
            .collect();
        assert_eq!(
            counting,
            ["UNSUPPORTED_URL", "HTTP_403", "NO_STREAM_FOUND", "VIDEO_UNRESOLVED"],
            "a new ErrorCode must be a conscious choice here"
        );
    }

    /// P11.4 review round 2: what the nightly yt-dlp task does with each verdict.
    /// Only a rollback marks the build rejected; a postponed update writes no
    /// marker, is Skipped and counts a postponed night; an unverified build is a
    /// warning and a failed task, never the INFO "updated" row.
    #[test]
    fn the_nightly_update_maps_each_verdict_honestly() {
        use omni_core::scheduler::TaskOutcome;
        let kept = ytdl_verdict_action(&UpdateVerdict::Kept { checked: 4 }, "stable", "0123456789abcdef");
        assert_eq!((kept.reject, kept.level, kept.outcome, kept.close_breakers, kept.postponed_night), (false, "INFO", TaskOutcome::Ok, true, Some(false)));
        assert!(kept.audit.starts_with("yt-dlp updated (stable channel, sha256 0123456789abcdef); 4 self-check link(s) still work"));

        let back = ytdl_verdict_action(&UpdateVerdict::RolledBack { broken: vec!["X video".into()] }, "stable", "0123456789abcdef");
        assert_eq!((back.reject, back.level, back.outcome, back.postponed_night), (true, "ERROR", TaskOutcome::Failed, Some(false)));
        assert!(back.audit.contains("could not get X video"));

        let later = ytdl_verdict_action(&UpdateVerdict::Postponed { why: "no youtube link could be compared".into() }, "stable", "0123456789abcdef");
        assert_eq!((later.reject, later.level, later.outcome, later.close_breakers, later.postponed_night), (false, "WARN", TaskOutcome::Skipped, false, Some(true)));
        assert!(later.audit.contains("postponed to the next night: no youtube link could be compared"));

        let unknown = ytdl_verdict_action(&UpdateVerdict::Unverified { why: "the swap failed".into() }, "stable", "0123456789abcdef");
        assert_eq!((unknown.reject, unknown.level, unknown.outcome, unknown.close_breakers, unknown.postponed_night), (false, "WARN", TaskOutcome::Failed, false, None));
        assert!(unknown.audit.contains("is live but could not be verified: the swap failed") && !unknown.audit.contains("updated"));
    }

    /// P11.4 review round 1: the admin's cookie «Test» while the platform cools
    /// down after a rate limit says to try again in a few minutes (an error the
    /// card shows), at once, and stores nothing about the jar.
    #[tokio::test(start_paused = true)]
    async fn the_admin_jar_test_during_a_cool_down_says_try_later_and_stores_nothing() {
        use omni_web::state::CookieTester;
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let jar = omni_core::cookies::parse_netscape("x.com", ".x.com\tTRUE\t/\tTRUE\t0\tn\tvalue-123456\n").unwrap();
        repo.put_cookie_jar("x.com", &jar, "admin", chrono::Utc::now()).unwrap();
        let pacer = omni_broadcast::pacing::HostPacer::default();
        pacer.penalize("x", omni_broadcast::pacing::RATE_LIMIT_COOL_DOWN);
        std::fs::create_dir_all(dir.path().join("temp")).unwrap();
        let tester = CookieJarTester { repo: repo.clone(), bin_dir: dir.path().join("bin"), temp_path: dir.path().join("temp"), pacer, ffmpeg: None };
        let t0 = tokio::time::Instant::now();
        let got = tester.test("x.com", "https://x.com/someone/status/1").await;
        assert_eq!(tokio::time::Instant::now(), t0);
        assert_eq!(got, Err(omni_broadcast::cookie_file::JAR_TEST_LIMITED.to_string()));
        assert_eq!(repo.list_cookie_jars().unwrap()[0].last_test_ok, None, "a result was stored");
    }
}
