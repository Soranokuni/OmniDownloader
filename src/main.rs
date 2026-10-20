use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use omni_broadcast::errors::ErrorCode;
use omni_broadcast::pipeline::BroadcastEngine;
use omni_browser::agent::{ComputerUseAgentPlaceholder, FileLockerResolver};
use omni_browser::evidence::{Evidence, EvidenceClass};
use omni_browser::sniffer::StreamSniffer;
use omni_core::config::{AppConfig, RepairMode};
use omni_core::paths::AppPaths;
use omni_core::dependencies::DependencyManager;
use omni_core::models::{JobStage, JobStatus};
use omni_core::repository::Repository;
use omni_core::secrets::SecretStore;
use omni_core::health::HealthState;
use omni_core::site_health::{SiteOutcome, SiteState};
use omni_core::logging::{LogGuard, Redactions};
use omni_core::update_gate::UpdateGate;
use omni_email::watcher::EmailWatcher;
use omni_web::server::WebServer;
use omni_web::state::AppState;

mod benchmark;
mod canary;
mod cookies_cli;
mod repair;
mod selfcheck;
mod site_memory;

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
        /// watchfolder); LocalSystem when not given
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
        /// Account the service runs as, e.g. `DOMAIN\svc_omni`. The password is
        /// prompted for. Omit for LocalSystem, which cannot authenticate to an
        /// SMB share (defect W-13) — the installer warns when the watchfolder
        /// is one.
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
            .unwrap_or_else(|| PathBuf::from(format!("{name}.exe")))
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
            })?;
        }
        Some(Commands::Setup) => {
            omni_cli::run_setup_wizard(Some(&cli.config))?;
        }
        Some(Commands::Install { .. } | Commands::Uninstall { .. }) => unreachable!("handled before paths"),
        Some(Commands::Service { action }) => {
            let sub = match action {
                ServiceAction::Install { account } => {
                    // The watchfolder is read from the config so the installer
                    // can tell whether LocalSystem will actually be able to
                    // deliver to it.
                    let watchfolder = AppConfig::load_from_file(&paths.config)
                        .ok()
                        .map(|c| c.watchfolder_path);
                    omni_cli::ServiceSubcommand::Install {
                        account,
                        watchfolder,
                    }
                }
                ServiceAction::Uninstall => omni_cli::ServiceSubcommand::Uninstall,
                ServiceAction::Start => omni_cli::ServiceSubcommand::Start,
                ServiceAction::Stop => omni_cli::ServiceSubcommand::Stop,
                ServiceAction::Status => omni_cli::ServiceSubcommand::Status,
            };
            omni_cli::handle_service_command(sub)?;
        }
        Some(Commands::Selfcheck) => {
            let config = AppConfig::load_from_file(&paths.config)
                .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
            let repo = Repository::new(paths.resolve(&config.database_path))?;
            let health = HealthState::new();
            let ytdl = DependencyManager::new(&paths.bin).ytdl_live();
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
            let mut failing = 0;
            for (link, outcome) in selfcheck::check_and_record(&repo, &ytdl, &links).await {
                if !outcome.ok {
                    failing += 1;
                }
                println!("  {} {:<32} {}", if outcome.ok { "✓" } else { "✗" }, link.label, outcome.detail);
            }
            println!("{} of {} links work.", links.len() - failing, links.len());
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
    service_shutdown_rx: Option<tokio::sync::mpsc::Receiver<()>>,
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
    if config.ytdl_path.is_none() {
        ensure_ytdl_folder_build(&dep_mgr, &config.ytdl_channel, config.ytdl_auto_update_nightly).await;
    }
    let ffmpeg_path = config
        .ffmpeg_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dep_mgr.find_binary("ffmpeg"))
        .unwrap_or_else(|| PathBuf::from("ffmpeg.exe"));
    let ffprobe_path = config
        .ffprobe_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dep_mgr.find_binary("ffprobe"))
        .unwrap_or_else(|| PathBuf::from("ffprobe.exe"));
    let bmxtranswrap_path = config
        .bmxtranswrap_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dep_mgr.find_binary("bmxtranswrap"))
        .unwrap_or_else(|| PathBuf::from("bmxtranswrap.exe"));
    let ytdl_path = config
        .ytdl_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dep_mgr.find_binary("yt-dlp"))
        .unwrap_or_else(|| PathBuf::from("yt-dlp.exe"));

    info!("Broadcast Toolchain:");
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
    let (st_ffmpeg, st_ffprobe, st_bmx) = (ffmpeg_path.clone(), ffprobe_path.clone(), bmxtranswrap_path.clone());
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
    .with_archive(Some(archive_settings.clone())));

    let (shutdown_tx, _) = broadcast::channel::<()>(16);

    // Keeps the nightly tool update from swapping yt-dlp.exe out from under a
    // running download (plan P2.9, defect D-16).
    let update_gate = UpdateGate::new();

    // Live subsystem health, shared with the web panels (plan P6.2, W-09).
    let health = HealthState::new();

    // Everything the self-test finds used to surface as a *job* failure,
    // attributed to whatever link happened to be at the front of the queue —
    // a missing bmxtranswrap.exe reported as a problem with someone's YouTube
    // URL, once per job. It never blocks start-up: refusing to come up is the
    // one outcome an operator cannot diagnose from the MCR desk.
    omni_core::selftest::run(&health, &bin_dir, &watchfolder_path, &temp_path).await;
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
    selfcheck::refresh_health(&repo, &health);
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
            .with_scratch(temp_path.clone()),
    );
    // The admin's Test URL: a dry run of the ladder; its world is set once the workers' exists.
    let url_tester = Arc::new(repair::AdminTester::new());
    omni_core::cookies::refresh_health(&repo, &health);
    // Only the Windows service locks its folders; a console run would lock the developer out.
    lock_down_install(service_shutdown_rx.is_some(), &[paths.data.clone(), temp_path.clone()], &health).await;
    let mut web_state = AppState::new(repo.clone(), config.clone(), config_path.to_path_buf())
        .with_url_tester(url_tester.clone())
        .with_cookie_tester(Arc::new(CookieJarTester {
            repo: repo.clone(),
            bin_dir: bin_dir.clone(),
            temp_path: temp_path.clone(),
        }))
        .with_site_runner(Arc::new(canary::AdminRunner::new(canary_env.clone())))
        .with_benchmark_starter(Arc::new(benchmark::Starter::new(Arc::new(benchmark::Benchmark::new(
            repo.clone(),
            paths.clone(),
            Arc::new(benchmark::RealStages::new(repo.clone(), bench_tools, paths.benchmark_temp())),
            "daemon",
        ).with_gate(update_gate.clone())))))
        .with_secret_store(secret_store.clone())
        .with_health(health.clone())
        .with_llm(live_llm.clone())
        .with_busy_jobs(busy_jobs.clone())
        .with_temp_dir(temp_path.clone())
        .with_events(events.clone());
    if let Some(r) = &log_redactions {
        web_state = web_state.with_redactions(r.clone());
    }
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
    match repo.recover_on_startup(&hostname) {
        Ok(0) => info!("No orphaned jobs to recover."),
        Ok(n) => warn!("Recovered {n} orphaned job(s) from a previous run; requeued."),
        Err(e) => error!("Orphan recovery failed: {e:?}"),
    }

    // 5. Sweep scratch directories left behind by jobs that are no longer
    // running. On a newsroom machine these are gigabytes of source media.
    match repo.running_job_ids() {
        Ok(running) => {
            let removed = omni_broadcast::delivery::WatchfolderDelivery::sweep_orphan_job_dirs(
                &temp_path.join("jobs"),
                &running,
            )
            .await;
            if removed > 0 {
                info!("Swept {removed} orphaned job workspace(s) from temp/jobs.");
            }
        }
        Err(e) => warn!("Could not list running jobs for the temp sweep: {e:?}"),
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
            llm: repair_llm_cfg,
        },
        env: Arc::new(repair_env),
    });
    // The overnight self-heal (P10.14) probes with the same ladder world as the workers.
    canary_env.set_heal(worker_repair.clone());
    url_tester.set_ctx(worker_repair.clone());
    let worker_canary = canary_env.clone();

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
            if !omni_core::selftest::check_disks(&worker_health, &worker_watchfolder, &worker_temp) {
                drop(permit);
                tokio::select! {
                    _ = worker_rx.recv() => break,
                    _ = tokio::time::sleep(Duration::from_secs(30)) => continue,
                }
            }

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

            let leased = if permit.1.is_some() {
                repo_worker.lease_job(&owner, LEASE_SECS)
            } else {
                // Every download slot is busy: an approved held video can
                // still start; anything else waits briefly for a slot.
                match repo_worker.lease_approved_held_job(&owner, LEASE_SECS) {
                    Ok(None) => match tokio::time::timeout(Duration::from_secs(1), semaphore.clone().acquire_owned()).await {
                        Ok(Ok(slot)) => {
                            permit.1 = Some(slot);
                            repo_worker.lease_job(&owner, LEASE_SECS)
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {
                            drop(permit);
                            continue;
                        }
                    },
                    other => other,
                }
            };
            let job = match leased {
                Ok(Some(job)) => job,
                Ok(None) => {
                    // Nothing ready. Release the permit and idle with jitter so
                    // several workers do not wake in lockstep and hammer SQLite.
                    drop(permit);
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

            let eng = engine_worker.clone();
            let rep = repo_worker.clone();
            let busy_guard = busy_worker.guard(job.id, &owner);
            let evidence_dir = worker_temp.join("evidence");
            let job_health = worker_health.clone();
            let job_canary = worker_canary.clone();
            let job_repair = worker_repair.clone();

            tokio::spawn(async move {
                // Held for the whole job, including every error path: dropping
                // it is what tells a waiting updater the downloader is idle.
                let _gate_pass = gate_pass;
                // Until this task is done with the job, MCR cannot queue it
                // again (a cancelled job stops only at its next stage).
                // Its token is the job run's: MCR's cancel and a lost lease
                // both fire it, and it kills the running tool (plan P1.15).
                let cancel = busy_guard.token();
                let _busy = busy_guard;
                let job_id = job.id;
                let (job_slot, download_slot) = permit;
                // Handed back on every exit, a panic in run_job included.
                let _slot_guard = download_slot.map(|slot| {
                    eng.hold_download_slot(job_id, &owner, slot);
                    eng.download_slot_guard(job_id, &owner)
                });

                // Keep the lease alive while we work. If it stops succeeding we
                // have lost the job -- the reaper requeued it, or an operator
                // cancelled it -- and must stop rather than deliver a file for a
                // job somebody else now owns.
                let heartbeat = {
                    let rep = rep.clone();
                    let owner = owner.clone();
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
                        tick.tick().await; // fires immediately; skip it
                        loop {
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                _ = tick.tick() => {
                                    match rep.heartbeat(job_id, &owner, LEASE_SECS) {
                                        Ok(true) => {}
                                        Ok(false) => {
                                            warn!("Job #{job_id}: lease lost, abandoning work");
                                            cancel.cancel();
                                            break;
                                        }
                                        Err(e) => warn!("Job #{job_id}: heartbeat failed: {e:?}"),
                                    }
                                }
                            }
                        }
                    })
                };

                let outcome = run_job(&rep, &eng, &owner, job, cancel.clone(), &evidence_dir, &job_repair, &job_health, &job_canary).await;
                cancel.cancel();
                heartbeat.abort();

                if let Err(e) = outcome {
                    // run_job handles its own failures; reaching here means the
                    // bookkeeping itself failed, so release the lease rather
                    // than leaving the job RUNNING until the reaper notices.
                    error!("Job #{job_id}: worker bookkeeping failed: {e:?}");
                    let _ = rep.finish(
                        job_id,
                        &owner,
                        JobStatus::RequiresReview,
                        Some(ErrorCode::PipelineFailed.as_str()),
                        Some(&e.to_string()),
                        None,
                    );
                }

                drop(job_slot);
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

    let _ = shutdown_tx.send(());
    tokio::time::sleep(Duration::from_millis(800)).await;
    info!("OmniDownloader shutdown complete. Goodbye!");

    Ok(())
}

/// Lease renewal interval and lease length.
///
/// The lease outlives several missed heartbeats so a momentarily busy machine
/// does not lose a job mid-transcode, but is short enough that a crashed worker
/// is noticed within a couple of minutes rather than at the next restart.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const LEASE_SECS: i64 = 180;
const LEASE_REAP_INTERVAL: Duration = Duration::from_secs(60);

/// A ready video waiting, or one step running, longer than this is a warning.
const QUEUE_SLOW_SECS: i64 = 30 * 60;

/// Once degraded, the check recovers only when both measures are under this,
/// so a backlog hovering near 30 min does not alarm on every edge.
const QUEUE_RECOVER_SECS: i64 = 25 * 60;

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

/// Whether this worker still holds the job. A failed check counts as lost:
/// better to stop than to queue or deliver for a job MCR may have cancelled.
fn still_ours(repo: &Repository, job_id: i64, owner: &str) -> bool {
    repo.owns_lease(job_id, owner).unwrap_or(false)
}

/// The run holding the lease parks the job for MCR's approval (P10.5): not
/// terminal, no lease, never leased again until approved. `false` when the
/// job is no longer this worker's.
fn finish_held(repo: &Repository, job_id: i64, owner: &str) -> Result<bool> {
    repo.finish(job_id, owner, JobStatus::AwaitingApproval, None, None, None)
}

/// An approved held video: deliver exactly that file (P10.5). No download,
/// no encode, no site-health or evidence bookkeeping. The terminal status is
/// set here, by the worker holding the lease, as for every other job.
async fn run_approved_delivery(
    repo: &Repository,
    engine: &Arc<BroadcastEngine>,
    owner: &str,
    job: &omni_core::models::Job,
    held: &omni_core::repository::HeldOutput,
    cancel: &CancellationToken,
) -> Result<()> {
    let job_id = job.id;
    let result = engine.deliver_held(job, owner, held, Some(cancel.clone())).await;
    if lost_lease(&result) {
        warn!("Job #{job_id}: no longer ours; nothing more to do");
        return Ok(());
    }
    match result {
        Ok(()) => {
            // The job and the held row finish together; a lost lease marks
            // nothing and keeps the held file.
            if repo.finish_held_delivery(job_id, owner)? {
                engine.discard_held_dir(job_id);
            } else {
                warn!("Job #{job_id}: no longer ours after delivering the approved video; the held file is kept");
            }
        }
        Err(e) => {
            let code = classify_pipeline_error(&e);
            let attempts = repo.get_job(job_id)?.map(|j| j.attempts).unwrap_or(1);
            let max_attempts = repo.get_job(job_id)?.map(|j| j.max_attempts).unwrap_or(3);
            let message = readable_cause(&e);
            let diagnostic: String = diagnostic_chain(&e).chars().take(8192).collect();
            let _ = repo.record_event(job_id, "DEBUG", None, &diagnostic);
            if code.is_retryable() && attempts < max_attempts {
                // The approval stands: the next lease delivers the same file.
                let backoff = code.backoff(attempts);
                info!("Job #{job_id}: {code} delivering the approved video, retrying in {} s", backoff.num_seconds());
                repo.requeue_after(job_id, owner, backoff, code.as_str(), &message)?;
            } else {
                if still_ours(repo, job_id, owner) {
                    repo.record_event(job_id, "ERROR", None, &omni_broadcast::errors::hint_for_job(code, &job.url, repo))?;
                }
                repo.finish(job_id, owner, JobStatus::RequiresReview, Some(code.as_str()), Some(&message), None)?;
            }
        }
    }
    Ok(())
}

/// Run one leased job through the pipeline.
///
/// Extracted from the worker loop so the lease, the heartbeat and the permit are
/// all managed in one place and cannot be forgotten on an early return.
async fn run_job(
    repo: &Repository,
    engine: &Arc<BroadcastEngine>,
    owner: &str,
    job: omni_core::models::Job,
    cancel: CancellationToken,
    evidence_dir: &std::path::Path,
    repair: &repair::RepairCtx,
    health: &HealthState,
    canary: &Arc<canary::CanaryEnv>,
) -> Result<()> {
    let repair_mode = repair.cfg.mode;
    let job_id = job.id;
    let orig_url = job.url.clone();

    // File lockers (WeTransfer and friends) are never handed to yt-dlp: the
    // link is a landing page, not a media URL. Plan P8 may resolve them
    // automatically; until then an operator fetches the file.
    let locker_resolver = ComputerUseAgentPlaceholder::new(None, None);
    if locker_resolver.can_handle(&orig_url) {
        warn!("Job #{job_id} is a file-locker link; routing to the MCR desk.");
        repo.record_event(
            job_id,
            "WARN",
            Some(JobStage::Extract),
            "File-locker link (WeTransfer/AirBridge/…); needs a manual download",
        )?;
        repo.finish(
            job_id,
            owner,
            JobStatus::ManualDownload,
            Some(ErrorCode::ManualDownload.as_str()),
            Some("File-locker link: download the file and use 'Upload file' on this job."),
            None,
        )?;
        return Ok(());
    }

    // MCR approved the video this job held back: deliver that file, and do
    // nothing of what follows (no download, no page scan, no sniffing).
    if let Some(held) = repo.approved_held(job_id)? {
        return run_approved_delivery(repo, engine, owner, &job, &held, &cancel).await;
    }

    // MCR asked for the next verified candidate of this job (P10.13): hold that
    // one, from the list the earlier run already verified. No download of the
    // link, no page scan, no sniffing, no recipes, no model.
    if let Some(attempt) = repair::open_next(repo, job_id)? {
        if !repo.set_stage(job_id, owner, JobStage::Download)? {
            warn!("Job #{job_id}: no longer ours; stopping without the next choice");
            return Ok(());
        }
        // Only `hold` lets MCR walk the verified list; with repair off or in shadow
        // nothing is downloaded and the job goes to review.
        if repair_mode != omni_core::config::RepairMode::Hold {
            let end = omni_core::repository::AttemptEnd {
                rung: Some("next".into()),
                outcome: Some("repair_off".into()),
                domain: attempt.domain.clone(),
                candidates_json: attempt.candidates_json.clone(),
                next_index: attempt.next_index,
                ..Default::default()
            };
            if let Err(e) = repo.finish_attempt(attempt.id, &end, chrono::Utc::now()) {
                warn!("Job #{job_id}: the next-choice attempt could not be closed: {e:#}");
            }
            repo.finish(
                job_id,
                owner,
                JobStatus::RequiresReview,
                Some(ErrorCode::VideoUnresolved.as_str()),
                Some(repair::REPAIR_OFF_EL),
                None,
            )?;
            return Ok(());
        }
        match repair::run_next(repair, &job, owner, evidence_dir, &attempt, &cancel).await {
            repair::NextResult::Held => {
                info!("Job #{job_id}: MCR's next choice is held for approval");
                finish_held(repo, job_id, owner)?;
            }
            repair::NextResult::Lost => warn!("Job #{job_id}: no longer ours; nothing more to do"),
            repair::NextResult::Failed => {
                if !cancel.is_cancelled() && still_ours(repo, job_id, owner) {
                    repo.record_event(job_id, "ERROR", None, ErrorCode::VideoUnresolved.hint_el())?;
                }
                repo.finish(
                    job_id,
                    owner,
                    JobStatus::RequiresReview,
                    Some(ErrorCode::VideoUnresolved.as_str()),
                    Some(repair::NEXT_FAILED_EL),
                    None,
                )?;
            }
        }
        return Ok(());
    }

    if !repo.set_stage(job_id, owner, JobStage::Download)? {
        warn!("Job #{job_id}: no longer ours; stopping without delivering");
        return Ok(());
    }

    // A word typed onto the end of the link that the parser did not know
    // ("…/arthro/-ΑΠΟΚΛΕΙΣΤΙΚΟ", plan P3.10): used only when the site says
    // the address as given does not exist and confirms the one without it.
    //
    // First the words the parser knows (P3.12): a job queued before P3.9
    // still carries "…-ΒΙΝΤΕΟ", and some sites (amna.gr) answer 200 for the
    // decorated address, so a 404 never says so. The same rule as the
    // parser: no guess, no request.
    let mut job = job;
    let orig_url = if orig_url.starts_with("http://") || orig_url.starts_with("https://") {
        let known = omni_email::parser::without_annotation(&orig_url);
        let (fixed, why) = if known != orig_url {
            (Some(known.to_string()), "the word the sender glued to it removed")
        } else {
            (omni_browser::pages::repair_dead_link(&orig_url).await, "it does not exist as sent (404); the site has it without the ending the sender added")
        };
        match fixed {
            Some(fixed) => {
                info!("Job #{job_id}: {orig_url} -> {fixed}");
                repo.record_event(job_id, "WARN", Some(JobStage::Extract), &format!("Link corrected, {why}: {fixed}"))?;
                if let Err(e) = repo.set_leased_job_url(job_id, owner, &fixed) {
                    warn!("Job #{job_id}: could not store the corrected link: {e:#}");
                }
                job.url = fixed.clone();
                fixed
            }
            None => orig_url,
        }
    } else {
        orig_url
    };

    // A news page yt-dlp reads as several videos (an article with four
    // Streamable embeds): this job takes the first, the others become
    // sibling jobs or offers, as when the sniffer finds them. Before, all
    // were downloaded into this job and one was delivered.
    let is_web = orig_url.starts_with("http://") || orig_url.starts_with("https://");
    let mut page_referer: Option<String> = None;
    // How many videos yt-dlp's page scan named (P10.2).
    let mut scanned_videos = 0usize;
    // The ones queued as this job's siblings, which a later sniff must not take as its own.
    // A retry starts from what an earlier attempt queued: those videos are neither taken nor
    // queued again, whichever of yt-dlp or the browser ranks one first now.
    let mut scan_siblings: Vec<String> = match repo.jobs_with_children(job_id) {
        Ok(jobs) => omni_broadcast::article::queued_children(&jobs, job_id),
        Err(e) => {
            warn!("Job #{job_id}: could not read the siblings already queued: {e:#}");
            Vec::new()
        }
    };
    // Every video the page offers is already queued as this job's sibling.
    let mut siblings_hold_the_rest = false;
    // What the page showed was only the site's furniture (P3.16): the pick yt-dlp made first was refused,
    // the one it made after the browser was refused (`Some`), and the addresses taken out, for the repair ladder.
    let mut pick_refused = false;
    let mut fallback_refused: Option<bool> = None;
    let mut furniture_urls: Vec<String> = Vec::new();
    // Held videos left out when furniture took the article's own: yt-dlp's pick after the browser may not take them.
    let mut refused_held: Vec<String> = Vec::new();
    // An embedded video on another site, whose cookie jar this job also carries (P3.5).
    let mut login_site: Option<String> = None;
    let mut _login_guard: Option<omni_broadcast::pipeline::LoginSiteGuard> = None;

    // A portal whose video one API request names (plan P3.8): no yt-dlp
    // page scan, no browser. A failed request just takes the usual way.
    if is_web {
        match omni_browser::pages::resolve(&orig_url).await {
            Ok(Some(video)) => {
                info!("Job #{job_id}: the page's own API names {video}");
                repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Video named by the site's API: {video}"))?;
                job.url = video;
                page_referer = Some(orig_url.clone());
            }
            Ok(None) => {}
            Err(e) => warn!("Job #{job_id}: site API lookup failed, trying the usual way: {e:#}"),
        }
    }

    if cancel.is_cancelled() {
        warn!("Job #{job_id}: cancelled; not scanning the page");
        return Ok(());
    }
    // The page scan, for a page neither the site's API nor a video platform named.
    let is_platform = omni_broadcast::downloader::is_video_platform(&orig_url);
    let mut scan = omni_broadcast::downloader::PageScan::default();
    if is_web && page_referer.is_none() && !is_platform {
        scan = engine.page_videos(&orig_url).await;
        scanned_videos = scan.videos.len();
    }
    // yt-dlp reads this page only through its generic scan, which does not know
    // where on the page each video sits: the browser chooses (P3.15), and yt-dlp's
    // own pick waits as the fallback.
    let route = omni_broadcast::downloader::first_attempt_resolved(
        page_referer.is_some(),
        is_platform,
        job.parent_job_id.is_some(),
        &scan,
        &orig_url,
        |u| async move { repair::final_page_url(&u).await },
    )
    .await;
    let sniff_first = route == omni_broadcast::downloader::FirstAttempt::SnifferFirst;
    let mut deferred_scan: Option<omni_broadcast::downloader::PageScan> = None;
    let mut process_result = if sniff_first {
        repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Extract),
            "yt-dlp reaches this page only through its generic scan; the browser chooses the video",
        )?;
        // The page's one video on another site: its cookie jar joins the job's for the
        // sniffed download too, as in `ytdlp_attempt` (P3.5).
        if let [only] = scan.videos.as_slice() {
            if other_site(&orig_url, only) {
                login_site = Some(only.clone());
                _login_guard = login_site.as_deref().map(|v| engine.set_login_site(job_id, v));
            }
        }
        deferred_scan = Some(scan);
        Err(anyhow::anyhow!("{}: yt-dlp reads the page only generically", ErrorCode::UnsupportedUrl.as_str())
            .context(ErrorCode::UnsupportedUrl.as_str()))
    } else {
        match ytdlp_attempt(repo, engine, owner, &mut job, &cancel, &orig_url, &scan_siblings, scan, page_referer.clone(), None, &[]).await? {
            None => return Ok(()),
            Some(a) => {
                page_referer = a.page_referer;
                scan_siblings.extend(a.scan_siblings);
                siblings_hold_the_rest = a.siblings_hold_the_rest;
                pick_refused = a.only_furniture;
                furniture_urls.extend(a.furniture_urls);
                login_site = a.login_site;
                _login_guard = a.login_guard;
                a.result
            }
        }
    };

    if lost_lease(&process_result) {
        warn!("Job #{job_id}: no longer ours; nothing more to do");
        return Ok(());
    }

    // yt-dlp could not resolve the page. For a news portal that is expected:
    // the video is behind an embedded player, so sniff the actual stream and
    // retry with the session context (referer/UA/cookies) it needs to avoid 403.
    // A platform post is sniffed only where that can help (`should_sniff`):
    // its page also plays other posts' videos.
    // What the browser saw, kept for a job that ends without a video (P10.1).
    let mut snapshot: Option<omni_browser::PageSnapshot> = None;
    let mut sniffed_streams: Vec<String> = Vec::new();
    // Every video the sniffer found is already queued as this job's sibling: the
    // job ends as NO_STREAM_FOUND for that reason, without the page assessment or
    // the repair ladder (they would look for, and could repair into, a second copy).
    // The referer the sniffed stream was tried with (the repair ladder does not retry it).
    let mut sniff_referer: Option<String> = None;
    let sniff = sniff_first
        || match &process_result {
            Err(e) => classify_pipeline_error(e).should_sniff(&orig_url),
            Ok(_) => false,
        };
    // The sniffer, taken first, found no usable video: yt-dlp's own pick is tried (P3.15).
    let mut sniff_end = SniffEnd::NotRun;
    if sniff && (orig_url.starts_with("http://") || orig_url.starts_with("https://")) {
        if sniff_first {
            info!("Job #{job_id}: yt-dlp reads the page only generically; the stream sniffer chooses.");
        } else {
            info!("Job #{job_id}: direct download failed; trying the stream sniffer.");
        }
        if !repo.set_stage(job_id, owner, JobStage::Extract)? {
            warn!("Job #{job_id}: no longer ours; not sniffing");
            return Ok(());
        }
        repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Extract),
            if sniff_first { "Sniffing the page for its video" } else { "Direct download failed; sniffing the page for a stream" },
        )?;

        if cancel.is_cancelled() {
            return Ok(());
        }
        let sniffed = StreamSniffer::extract_with_evidence(&orig_url, 25).await;
        snapshot = Some(sniffed.snapshot);
        let early_end = sniff_result_end(
            sniff_first,
            sniffed.media.is_ok(),
            snapshot.as_ref().and_then(|s| s.final_url.as_deref()),
        );
        match sniffed.media {
            Ok(_) if early_end == Some(SniffEnd::LandedOnPlatform) => {
                sniff_end = SniffEnd::LandedOnPlatform;
                repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Extract),
                    "The browser ended on a video platform's page; leaving the choice to yt-dlp",
                )?;
            }
            Ok(bundle) => {
                // An article's embedded posts are not all videos: the first
                // X post in a news247 article was a photo, became the job's
                // video, and sent the job to review while its two real
                // videos were delivered as siblings (P3.13). Ask yt-dlp
                // about each post first; keep the ones it cannot rule out.
                // Every video the browser saw is remembered for this article of the site,
                // then the ones the site shows on its other articles too (a promo, a show, a
                // recommendation) leave the candidates: never the job's video, never queued,
                // only offered (P3.16). An article's sibling job holds a stream or post, not
                // a page of the site.
                let site_memory::SniffScreen { pool, furniture, removed_own } = site_memory::screen_sniff(
                    repo,
                    &orig_url,
                    snapshot.as_ref(),
                    &bundle.all_streams,
                    job.parent_job_id.is_some(),
                    |u| omni_browser::sniffer::is_demoted(&bundle.meta, u),
                );
                furniture_urls.extend(furniture.iter().map(|f| f.url.clone()));
                let mut streams = without_videoless_posts(repo, engine, job_id, &pool, &cancel).await;
                if cancel.is_cancelled() {
                    return Ok(());
                }
                // Not one already queued as this job's sibling: it would air twice.
                let (left, queued_emptied) = omni_broadcast::article::remaining_after_queued(&streams, &scan_siblings);
                streams = left;
                // Furniture took the article's own video and no own one is left: a rail or header video is not
                // the article's either (P3.16).
                let (left, dropped_held) =
                    site_memory::leave_nothing_held(removed_own, streams, |u| omni_browser::sniffer::is_demoted(&bundle.meta, u));
                streams = left;
                furniture_urls.extend(dropped_held.iter().cloned());
                refused_held.extend(dropped_held);
                // Only what is left after the photo-only posts are dropped
                // says the page has a video.
                sniffed_streams = streams.clone();
                let primary = if streams.contains(&bundle.primary_stream) {
                    Some(bundle.primary_stream.clone())
                } else {
                    streams.first().cloned()
                };
                // The job's video among the candidates, in the sniffer's rank
                // order: the page's own first, a held one (outside the article,
                // a link outside its body, a contextual list) only when no
                // other exists, and a live platform video never (P1.18). With
                // nothing live this is the first candidate, as the ranking
                // already made it.
                let mut all_live = false;
                let mut all_dead: Option<ErrorCode> = None;
                let mut primary = primary;
                if primary.is_some() {
                    // Every kind of candidate is asked before the primary is decided (P3.15):
                    // a player page or stream that is gone is skipped for the next one, and
                    // no sibling or offer is planned until the primary is confirmed. With the
                    // sniffer's own session, as the download would have it.
                    let cands = omni_broadcast::downloader::sniffed_cands(&streams, |u| omni_browser::sniffer::is_demoted(&bundle.meta, u));
                    // The candidates were asked with the sniffer's referer: the repair ladder
                    // does not retry them with it again.
                    sniff_referer = Some(bundle.referer.clone());
                    let session = &bundle;
                    let choice = omni_broadcast::downloader::choose_primary(&cands, omni_broadcast::downloader::MAX_LIVE_PROBES, |u| {
                        let cancel = &cancel;
                        async move {
                            tokio::select! {
                                p = engine.probe_candidate(&u, Some(&session.referer), Some(&session.user_agent), session.cookies.as_deref()) => p,
                                _ = cancel.cancelled() => omni_broadcast::downloader::Probe::Unknown,
                            }
                        }
                    })
                    .await;
                    use omni_broadcast::downloader::Choice;
                    let (live, dead) = match choice {
                        Choice::Primary { url, live, dead } => {
                            primary = Some(url);
                            (live, dead)
                        }
                        Choice::AllLive { live } => {
                            all_live = true;
                            primary = None;
                            (live, Vec::new())
                        }
                        Choice::AllDead { code, live } => {
                            all_dead = Some(code);
                            primary = None;
                            (live, Vec::new())
                        }
                        Choice::Nothing => (Vec::new(), Vec::new()),
                    };
                    for l in &live {
                        info!("Job #{job_id}: {} is live; not taken", omni_core::cookies::host_and_path(l));
                        repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Live stream skipped: {l}"))?;
                    }
                    for d in &dead {
                        info!("Job #{job_id}: {} is gone; not taken", omni_core::cookies::host_and_path(d));
                        // A warning on the desk, so a video wrongly read as gone is seen.
                        repo.record_event(job_id, "WARN", Some(JobStage::Extract), &format!("Video that is gone skipped: {d}"))?;
                    }
                    streams.retain(|u| !live.contains(u) && !dead.contains(u));
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                }
                // The site's furniture is offered to MCR whatever the job takes; when the job
                // queues the article's siblings they write the offered list first, so then
                // it is done after them.
                if primary.is_none() {
                    omni_broadcast::article::offer_site_wide(repo, &job, &furniture);
                }
                match primary {
                    Some(primary) => {
                        // Found: no fallback even when this download fails. yt-dlp's own pick is
                        // the promo clip the browser was asked to avoid; a retry or review beats it.
                        sniff_end = SniffEnd::Found;
                        // The yt-dlp attempt may have found every scanned video already
                        // queued; the browser found one more, so a failure from here is
                        // that video's own and gets the assessment and the ladder.
                        siblings_hold_the_rest = false;
                        info!("Job #{job_id}: sniffer found {primary}");
                        repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Sniffed stream: {primary}"))?;
                        let mut retry_job = job.clone();
                        if !still_ours(repo, job_id, owner) {
                            warn!("Job #{job_id}: no longer ours; not queuing the article's other videos");
                            return Ok(());
                        }
                        let facts = article_facts(engine, &retry_job, &primary, &streams, &bundle.meta, &cancel).await;
                        if !still_ours(repo, job_id, owner) {
                            warn!("Job #{job_id}: no longer ours; not queuing the article's other videos");
                            return Ok(());
                        }
                        omni_broadcast::article::queue_article_siblings(repo, owner, &mut retry_job, &primary, &streams, &facts);
                        omni_broadcast::article::offer_site_wide(repo, &retry_job, &furniture);
                        retry_job.url = primary;
                        sniff_referer = Some(bundle.referer.clone());
                        if !repo.set_stage(job_id, owner, JobStage::Download)? {
                            warn!("Job #{job_id}: no longer ours; not downloading the sniffed stream");
                            return Ok(());
                        }
                        process_result = engine
                            .process_job_mode(
                                retry_job,
                                owner,
                                Some(&bundle.referer),
                                Some(&bundle.user_agent),
                                bundle.cookies.as_deref(),
                                Some(cancel.clone()),
                                omni_broadcast::pipeline::DeliveryMode::Deliver,
                            )
                            .await;
                    }
                    None if all_dead.is_some() => {
                        // Every video the browser found is gone: the job ends as a failed
                        // download with that code would, so the page assessment and the repair
                        // ladder see it as before. yt-dlp's own pick is not tried.
                        let code = all_dead.unwrap_or(ErrorCode::NoStreamFound);
                        sniff_end = SniffEnd::AllDead;
                        repo.record_event(
                            job_id,
                            "ERROR",
                            Some(JobStage::Extract),
                            &format!("Every video the browser found is gone ({})", code.as_str()),
                        )?;
                        process_result = Err(anyhow::anyhow!("{}: every video the browser found is gone", code.as_str())
                            .context(code.as_str()));
                    }
                    None if all_live => {
                        sniff_end = SniffEnd::AllLive;
                        repo.record_event(job_id, "ERROR", Some(JobStage::Extract), "Every video found on the page is a live stream")?;
                        process_result = Err(anyhow::anyhow!("{}: every video on the page is live", ErrorCode::LiveStream.as_str())
                            .context(ErrorCode::LiveStream.as_str()));
                    }
                    None if queued_emptied => {
                        sniff_end = SniffEnd::SiblingsHoldTheRest;
                        siblings_hold_the_rest = true;
                        repo.record_event(
                            job_id,
                            "ERROR",
                            Some(JobStage::Extract),
                            "The page's remaining videos are already queued as this job's siblings",
                        )?;
                        process_result = Err(anyhow::anyhow!(
                            "{}: the page's remaining videos are already queued as siblings",
                            ErrorCode::NoStreamFound.as_str()
                        )
                        .context(ErrorCode::NoStreamFound.as_str()));
                    }
                    None if site_memory::ends_on_furniture(&furniture, &streams) => {
                        // What the page showed is the site's own furniture (P3.16): no video for
                        // this job. yt-dlp's own pick is asked next (it is refused if it is the same
                        // promo), and a failure after that is assessed and repaired as any other.
                        sniff_end = SniffEnd::SiteWide;
                        repo.record_event(
                            job_id,
                            "ERROR",
                            Some(JobStage::Extract),
                            "The only videos on this page are ones the site shows on its other articles",
                        )?;
                        process_result = Err(anyhow::anyhow!(
                            "{}: the only videos on the page are ones the site shows on its other articles",
                            ErrorCode::NoStreamFound.as_str()
                        )
                        .context(ErrorCode::NoStreamFound.as_str()));
                    }
                    None => {
                        sniff_end = SniffEnd::PostsHaveNoVideo;
                        repo.record_event(
                            job_id,
                            "ERROR",
                            Some(JobStage::Extract),
                            "The page's embedded posts have no video (photos or text only)",
                        )?;
                        process_result = Err(anyhow::anyhow!(
                            "{}: the page's embedded posts have no video",
                            ErrorCode::NoStreamFound.as_str()
                        )
                        .context(ErrorCode::NoStreamFound.as_str()));
                    }
                }
            }
            Err(sniff_err) => {
                warn!("Job #{job_id}: sniffer found nothing: {sniff_err}");
                sniff_end = SniffEnd::Error;
                repo.record_event(
                    job_id,
                    "ERROR",
                    Some(JobStage::Extract),
                    &format!("Sniffer found no stream: {sniff_err}"),
                )?;
                if let Err(previous) = std::mem::replace(&mut process_result, Ok(omni_broadcast::pipeline::RunOutcome::Delivered)) {
                    process_result = Err(after_failed_sniff(previous, sniff_err));
                }
            }
        }
    }

    // The browser, asked first, found no video: yt-dlp's own pick, as it would have
    // been taken before. Its result takes the error handling below; the sniff's
    // snapshot stays as evidence. Not after a live stream, siblings that hold the
    // rest, a cancel or a lost lease.
    let end = if lost_lease(&process_result) {
        SniffEnd::LeaseLost
    } else if cancel.is_cancelled() {
        SniffEnd::Cancelled
    } else {
        sniff_end
    };
    if falls_back_to_ytdlp(sniff_first, end) {
        repo.record_event(job_id, "INFO", Some(JobStage::Extract), "The browser found no video; trying yt-dlp's own pick")?;
        if !repo.set_stage(job_id, owner, JobStage::Download)? {
            warn!("Job #{job_id}: no longer ours; not trying yt-dlp's own pick");
            return Ok(());
        }
        // The login guard is the attempt's to make again; two for one job would drop the new one.
        _login_guard = None;
        let scan = deferred_scan.take().unwrap_or_default();
        match ytdlp_attempt(repo, engine, owner, &mut job, &cancel, &orig_url, &scan_siblings, scan, page_referer.clone(), snapshot.as_ref().and_then(|s| s.final_url.as_deref()), &refused_held).await? {
            None => return Ok(()),
            Some(a) => {
                page_referer = a.page_referer;
                siblings_hold_the_rest = a.siblings_hold_the_rest;
                fallback_refused = Some(a.only_furniture);
                furniture_urls.extend(a.furniture_urls);
                login_site = a.login_site;
                _login_guard = a.login_guard;
                process_result = a.result;
            }
        }
    }

    if lost_lease(&process_result) {
        warn!("Job #{job_id}: no longer ours; nothing more to do");
        return Ok(());
    }

    match process_result {
        Ok(omni_broadcast::pipeline::RunOutcome::Held) => {
            // Made and checked, but held for MCR (P10.5): nothing was delivered.
            finish_held(repo, job_id, owner)?;
            Ok(())
        }
        Ok(omni_broadcast::pipeline::RunOutcome::Delivered) => {
            let delivered = repo
                .get_job(job_id)?
                .and_then(|j| j.file_path)
                .filter(|p| !p.is_empty());
            let done = repo.finish(
                job_id,
                owner,
                JobStatus::Completed,
                None,
                None,
                delivered.as_deref(),
            )?;
            if done && !cancel.is_cancelled() {
                // An article's sibling job holds a sniffed stream or post,
                // not a page of the site: it says nothing about the site.
                if job.parent_job_id.is_none() {
                    note_site_outcome(repo, health, job_id, &orig_url, SiteOutcome::Success);
                }
            }
            Ok(())
        }
        Err(e) => {
            // The classified code decides whether waiting could possibly help
            // (plan P1.9). Retrying a deleted video burns worker slots and
            // delays telling the journalist their link is dead; retrying a
            // dropped connection usually just works.
            let mut code = classify_pipeline_error(&e);
            let attempts = repo.get_job(job_id)?.map(|j| j.attempts).unwrap_or(1);
            let max_attempts = repo.get_job(job_id)?.map(|j| j.max_attempts).unwrap_or(3);
            let mut message = readable_cause(&e);

            // The full context chain, which carries each stage's stderr tail,
            // into the job's own timeline (plan P6.1). `error_message` is the
            // one line the MCR card shows; this is what an engineer needs an
            // hour later, and putting it on the job means it survives log
            // rotation and does not require finding the right file.
            let diagnostic: String = diagnostic_chain(&e).chars().take(8192).collect();
            let _ = repo.record_event(job_id, "DEBUG", None, &diagnostic);

            if code.is_retryable() && attempts < max_attempts {
                let backoff = code.backoff(attempts);
                info!(
                    "Job #{job_id}: {code}, retrying in {} s (attempt {attempts}/{max_attempts})",
                    backoff.num_seconds()
                );
                repo.requeue_after(job_id, owner, backoff, code.as_str(), &message)?;
            } else {
                // MANUAL_DOWNLOAD is not a failure, it is a different workflow.
                let status = match code {
                    ErrorCode::ManualDownload => JobStatus::ManualDownload,
                    _ => JobStatus::RequiresReview,
                };
                // A page that gave no video: what did it show (P10.2)? May
                // name a more exact code, and may look at the page again.
                let original = code;
                if is_web
                    && matches!(code, ErrorCode::NoStreamFound | ErrorCode::UnsupportedUrl | ErrorCode::Http403)
                    && !omni_broadcast::downloader::is_video_platform(&orig_url)
                    && !siblings_hold_the_rest
                {
                    let page = FailedPage {
                        url: &orig_url,
                        mode: repair_mode,
                        snapshot: snapshot.take(),
                        sniffed_streams: &sniffed_streams,
                        original: code,
                        named_by_site: site_named_video(page_referer.is_some(), scanned_videos),
                    };
                    let (exact, line, seen) = assess_failed_page(repo, owner, job_id, &cancel, page).await;
                    code = exact;
                    if let Some(line) = line {
                        message = line;
                    }
                    snapshot = seen;
                }
                let wants_evidence = |code: ErrorCode| {
                    matches!(original, ErrorCode::NoStreamFound | ErrorCode::UnsupportedUrl | ErrorCode::Http403)
                        || matches!(
                            code,
                            ErrorCode::DrmProtected
                                | ErrorCode::NoVideoOnPage
                                | ErrorCode::VideoUnresolved
                                | ErrorCode::RepairExhausted
                        )
                };

                // The page showed a video the normal path could not resolve:
                // the repair ladder (P10.11a). Only here, only for a web page,
                // only with repair on; a held video waits for MCR.
                let tried = repair::Tried::of(&job, &orig_url, page_referer.as_deref(), &sniffed_streams, sniff_referer.as_deref());
                let ladder = if siblings_hold_the_rest {
                    None
                } else {
                    repair::after_failure_with(repair, &job, owner, &orig_url, code, &snapshot, tried, furniture_urls.clone(), &cancel).await
                };
                if let Some(ladder) = ladder {
                    match &ladder {
                        repair::LadderResult::ShadowFound { verified } => {
                            info!("Job #{job_id}: repair (shadow) verified {} video(s); the job ends as before", verified.len())
                        }
                        repair::LadderResult::Held { recipe_hash } => {
                            info!("Job #{job_id}: a repaired video is held for MCR (recipe {})", &recipe_hash[..recipe_hash.len().min(8)])
                        }
                        _ => {}
                    }
                    match repair::disposition(&ladder, repair_mode, repair.cfg.budget) {
                        repair::Disposition::Held => {
                            if let Some(snap) = &snapshot {
                                if wants_evidence(code) && still_ours(repo, job_id, owner) {
                                    save_page_evidence(repo, evidence_dir, job_id, snap).await;
                                }
                            }
                            // Waiting, not failed: no site-health record.
                            finish_held(repo, job_id, owner)?;
                            return Ok(());
                        }
                        repair::Disposition::Lost => {
                            warn!("Job #{job_id}: no longer ours; nothing more to do");
                            return Ok(());
                        }
                        repair::Disposition::Finish { code: c, message: m } => {
                            code = c;
                            message = m;
                        }
                        // The model said why nothing can be resolved: MCR reads it on the card.
                        repair::Disposition::Annotate(reason) => message = repair::annotate(&message, &reason),
                        repair::Disposition::Unchanged => {}
                    }
                }

                // Nothing more is written for a job that is no longer ours.
                if !cancel.is_cancelled() && still_ours(repo, job_id, owner) {
                    repo.record_event(job_id, "ERROR", None, &omni_broadcast::errors::hint_for_job(code, login_site.as_deref().unwrap_or(&job.url), repo))?;
                }
                // Only the job's owner writes evidence: a job MCR cancelled
                // or the reaper gave away has no business leaving a file.
                if let Some(snap) = &snapshot {
                    if wants_evidence(code) && !cancel.is_cancelled() && still_ours(repo, job_id, owner) {
                        save_page_evidence(repo, evidence_dir, job_id, snap).await;
                    }
                }
                let done = repo.finish(
                    job_id,
                    owner,
                    status,
                    Some(code.as_str()),
                    Some(&message),
                    None,
                )?;
                // Site health (P10.3): only a finished job, and only the
                // codes that say something about the site, not this page.
                // A repair that ran out of time counts as the unresolved video it was.
                let site_code = repair::site_code(code);
                if site_failure_counts(done, cancel.is_cancelled(), job.parent_job_id.is_some(), furniture_only_end(pick_refused, end, fallback_refused), site_code.as_str())
                {
                    let noted = note_site_outcome(
                        repo,
                        health,
                        job_id,
                        &orig_url,
                        SiteOutcome::SiteFailure(site_code.as_str().to_string()),
                    );
                    // A site that just started failing: look at its recent
                    // articles in the background (P10.4). Never awaited here.
                    if let Some((domain, before, after)) = noted {
                        if canary::should_trigger(repair_mode, before, after) {
                            canary.trigger(&domain);
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

/// Fold a finished job into its site's health (plan P10.3). Bookkeeping only:
/// a database error is a warning and never changes the job's outcome.
fn note_site_outcome(
    repo: &Repository,
    health: &HealthState,
    job_id: i64,
    url: &str,
    outcome: SiteOutcome,
) -> Option<(String, SiteState, SiteState)> {
    let is_platform = omni_broadcast::downloader::is_video_platform(url);
    let domain = omni_core::site_health::site_for_job(url, is_platform)?;
    match repo.record_site_outcome(&domain, outcome) {
        Ok((before, after)) => {
            if before != after {
                let line = match (after, repo.get_site(&domain)) {
                    (SiteState::Healthy, _) => format!("Site {domain} is healthy again"),
                    (state, Ok(Some(row))) => format!(
                        "Site {domain} is now {} ({} failures in a row)",
                        state.as_str(),
                        row.consecutive_fail
                    ),
                    (state, _) => format!("Site {domain} is now {}", state.as_str()),
                };
                info!("Job #{job_id}: {line}");
                let _ = repo.record_event(job_id, "INFO", None, &line);
            }
            omni_core::site_health::refresh_health(repo, health);
            Some((domain, before, after))
        }
        Err(e) => {
            warn!("Job #{job_id}: site health for {domain} not recorded: {e:#}");
            None
        }
    }
}

/// Whether to open the page again with the diagnostic recorders: only when
/// repair is on, for a page (not a platform post), and when the passive look
/// proved nothing (no snapshot at all, weak or none).
fn should_diagnose(mode: RepairMode, passive: Option<EvidenceClass>, is_platform: bool) -> bool {
    mode != RepairMode::Off
        && !is_platform
        && passive.map_or(true, |c| matches!(c, EvidenceClass::Weak | EvidenceClass::None))
}

/// The code and the one-line reason (Greek, for the MCR card) a page's
/// evidence gives a job that failed with `original`. `None` for the line
/// keeps the existing cause text. A "no video" verdict needs the diagnostic
/// run; without it the original code stands, as it does for a weak page.
fn evidence_code(original: ErrorCode, ev: &Evidence) -> (ErrorCode, Option<String>) {
    match ev.class {
        EvidenceClass::Protected => {
            let system = ev.signals.iter().find_map(|s| s.strip_prefix("eme:")).unwrap_or("DRM");
            (ErrorCode::DrmProtected, Some(format!("Το βίντεο έχει DRM ({system})")))
        }
        EvidenceClass::LoginWall => (ErrorCode::LoginRequired, None),
        EvidenceClass::GeoBlocked => (ErrorCode::GeoBlocked, None),
        EvidenceClass::Strong => (
            ErrorCode::VideoUnresolved,
            Some(format!("Βρέθηκε βίντεο στη σελίδα ({}) αλλά δεν κατέβηκε", ev.signals.join(", "))),
        ),
        EvidenceClass::None if ev.diagnosed => {
            (ErrorCode::NoVideoOnPage, Some("Δεν βρέθηκε βίντεο ούτε στον δεύτερο έλεγχο".to_string()))
        }
        EvidenceClass::None | EvidenceClass::Weak => (original, None),
    }
}

/// What a failed web job's page left behind, for [`assess_failed_page`].
struct FailedPage<'a> {
    url: &'a str,
    mode: RepairMode,
    snapshot: Option<omni_browser::PageSnapshot>,
    sniffed_streams: &'a [String],
    original: ErrorCode,
    /// The site's API or yt-dlp's page scan named a video for this job.
    named_by_site: bool,
}

/// How the sniffer's run ended for a job whose page it was asked about first (P3.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SniffEnd {
    /// The sniffer did not run.
    NotRun,
    /// It chose a video; whatever the download does, yt-dlp is not asked.
    Found,
    /// It failed or found no stream.
    Error,
    /// Its streams were embedded posts with no video in them.
    PostsHaveNoVideo,
    /// Every video it found is live.
    AllLive,
    /// Every video it found is gone (probed): the job ends with that code, yt-dlp's pick is not tried.
    AllDead,
    /// What it found is already queued as this job's siblings.
    SiblingsHoldTheRest,
    /// Every video it found is one the site shows on its other articles (P3.16), or only held ones
    /// were left once those were out: none is the job's. yt-dlp's own pick is asked next, and refused
    /// if it is one of them (`guard_scan`).
    SiteWide,
    Cancelled,
    LeaseLost,
    /// The browser ended on a video platform's page (a meta refresh or script redirect): yt-dlp
    /// follows it and takes the post itself, as before.
    LandedOnPlatform,
}

/// How a sniff ends before its streams are looked at: it failed, or (for a page the browser was
/// asked about first) it ended on a video platform's page, which yt-dlp follows instead.
/// `None`: the streams it found are looked at.
fn sniff_result_end(sniff_first: bool, media_ok: bool, final_url: Option<&str>) -> Option<SniffEnd> {
    if !media_ok {
        return Some(SniffEnd::Error);
    }
    (sniff_first && omni_broadcast::downloader::landed_on_platform(final_url)).then_some(SniffEnd::LandedOnPlatform)
}

/// Whether yt-dlp's own pick is tried after the sniffer, asked first, ended as `end`: only when
/// it found no usable video (its only videos were the site's furniture: yt-dlp's pick is asked, and
/// refused if it is one of them). Never after a live stream, siblings that hold the rest, a video
/// it did choose, a cancel or a lost lease.
fn falls_back_to_ytdlp(sniff_first: bool, end: SniffEnd) -> bool {
    sniff_first && matches!(end, SniffEnd::Error | SniffEnd::PostsHaveNoVideo | SniffEnd::LandedOnPlatform | SniffEnd::SiteWide)
}

/// What yt-dlp's attempt at a web page left: its result, and what the page scan
/// set up for it (the first video's referer, the siblings queued, the other site's login).
struct YtdlpAttempt {
    result: Result<omni_broadcast::pipeline::RunOutcome>,
    page_referer: Option<String>,
    scan_siblings: Vec<String>,
    /// Every video of the page's scan is already queued as a sibling: nothing was downloaded.
    siblings_hold_the_rest: bool,
    /// The page's only videos are the site's furniture (P3.16): not a failure of the site.
    only_furniture: bool,
    /// What was taken out of the scan as the site's furniture.
    furniture_urls: Vec<String>,
    login_site: Option<String>,
    login_guard: Option<omni_broadcast::pipeline::LoginSiteGuard>,
}

/// yt-dlp's own attempt at a job's link: the page scan's pick among several videos
/// (the rest become siblings), or the download of the link itself. Taken first
/// for most links, and after a sniff that found nothing for a page yt-dlp reads
/// only generically (P3.15). `Ok(None)`: the job is no longer ours or was
/// cancelled; nothing more to do.
async fn ytdlp_attempt(
    repo: &Repository,
    engine: &Arc<BroadcastEngine>,
    owner: &str,
    job: &mut omni_core::models::Job,
    cancel: &CancellationToken,
    orig_url: &str,
    queued: &[String],
    scan: omni_broadcast::downloader::PageScan,
    page_referer: Option<String>,
    // Where the browser ended, when it looked at the page first (its site is looked up too).
    browser_final: Option<&str>,
    // Held videos the browser's look took out (furniture removed the article's own): not yt-dlp's pick either.
    refused: &[String],
) -> Result<Option<YtdlpAttempt>> {
    let job_id = job.id;
    let mut page_referer = page_referer;
    let mut scan_siblings: Vec<String> = Vec::new();
    // An embedded video on another site, whose cookie jar this job also carries (P3.5).
    let mut login_site: Option<String> = None;
    // Every video of the page's scan was live (P1.18).
    let mut all_live_scan = false;
    let known_unsupported = scan.unsupported;
    let scan_count = scan.videos.len();
    // Videos an earlier attempt queued as siblings are neither taken nor queued again.
    let (mut videos, held_by_siblings) = omni_broadcast::article::remaining_after_queued(&scan.videos, queued);
    // A video the site shows on its other articles too is never yt-dlp's pick (P3.16): out of the
    // multi-video choice, and a single result whose media is one is not downloaded. Read only: the
    // browser's sightings are what is remembered.
    let mut furniture: Vec<omni_core::site_videos::Furniture> = Vec::new();
    let mut only_furniture = false;
    if job.parent_job_id.is_none() && page_referer.is_none() {
        let guard = site_memory::guard_scan(repo, orig_url, scan.webpage_url.as_deref(), browser_final, std::mem::take(&mut videos), &scan.media_urls, refused);
        (videos, furniture, only_furniture) = (guard.videos, guard.furniture, guard.only_furniture);
    }
    if scan_count > 1 && !videos.is_empty() {
        // yt-dlp's page scan says nothing about where on the page a video
        // sat, so the placement rule (R2) is the sniffer's alone; here only
        // a glomex contextual list is known (held), and live videos are
        // skipped: one chosen or queued as a sibling would air twice once
        // the sniffer ran for the failed job (P1.18).
        let (cands, scan_meta) = page_scan_cands(&videos);
        let choice = omni_broadcast::downloader::choose_primary(&cands, omni_broadcast::downloader::MAX_LIVE_PROBES, |u| {
            let cancel = &cancel;
            async move {
                tokio::select! {
                    i = engine.video_length(&u) => omni_broadcast::downloader::Probe::from_length(i),
                    _ = cancel.cancelled() => omni_broadcast::downloader::Probe::Unknown,
                }
            }
        })
        .await;
        if cancel.is_cancelled() {
            return Ok(None);
        }
        use omni_broadcast::downloader::Choice;
        let (first, live) = match choice {
            Choice::Primary { url, live, .. } => (Some(url), live),
            Choice::AllLive { live } => (None, live),
            // A length probe never says a video is gone, so this cannot happen here.
            Choice::AllDead { live, .. } => (None, live),
            Choice::Nothing => (None, Vec::new()),
        };
        for l in &live {
            info!("Job #{job_id}: {} is live; not taken", omni_core::cookies::host_and_path(l));
            repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Live stream skipped: {l}"))?;
        }
        videos.retain(|u| !live.contains(u));
        match first {
            None => {
                all_live_scan = true;
                videos.clear();
            }
            Some(first) => {
                info!("Job #{job_id}: the page holds {} videos; this job takes the first", videos.len());
                repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Extract),
                    &format!("The page holds {} videos; this job downloads the first: {}", videos.len(), first),
                )?;
                if !still_ours(repo, job_id, owner) {
                    warn!("Job #{job_id}: no longer ours; not queuing the article's other videos");
                    return Ok(None);
                }
                let facts = article_facts(engine, &job, &first, &videos, &scan_meta, &cancel).await;
                let count = job.max_videos.filter(|n| *n > 0).map(|n| n as usize);
                scan_siblings =
                    omni_broadcast::article::plan_article_videos_facts(&job.url, &first, &videos, &job.index_str, count, &facts)
                        .siblings
                        .into_iter()
                        .map(|(u, _)| u)
                        .collect();
                omni_broadcast::article::queue_article_siblings(repo, owner, job, &first, &videos, &facts);
                job.url = first;
                page_referer = Some(orig_url.to_string());
            }
        }
    } else if scan_count == 1 && videos.len() == 1 && other_site(&orig_url, &videos[0]) {
        // The article's one video is another site's (an X post, say): that site's
        // cookie jar joins the job's, and its name is in the login hint. The
        // download itself is unchanged.
        login_site = Some(videos[0].clone());
    }
    if !still_ours(repo, job_id, owner) {
        warn!("Job #{job_id}: no longer ours; not downloading");
        return Ok(None);
    }
    // After the article's siblings were queued, which write the offered list first.
    omni_broadcast::article::offer_site_wide(repo, job, &furniture);
    let login_guard = login_site.as_deref().map(|v| engine.set_login_site(job_id, v));
    let result = if only_furniture && !all_live_scan && !held_by_siblings {
        repo.record_event(
            job_id,
            "ERROR",
            Some(JobStage::Extract),
            "The only videos on this page are ones the site shows on its other articles",
        )?;
        Err(anyhow::anyhow!(
            "{}: the only videos on the page are ones the site shows on its other articles",
            ErrorCode::NoStreamFound.as_str()
        )
        .context(ErrorCode::NoStreamFound.as_str()))
    } else if all_live_scan {
        repo.record_event(job_id, "ERROR", Some(JobStage::Extract), "Every video found on the page is a live stream")?;
        Err(anyhow::anyhow!("{}: every video on the page is live", ErrorCode::LiveStream.as_str())
            .context(ErrorCode::LiveStream.as_str()))
    } else if held_by_siblings {
        repo.record_event(
            job_id,
            "ERROR",
            Some(JobStage::Extract),
            "The page's remaining videos are already queued as this job's siblings",
        )?;
        Err(anyhow::anyhow!("{}: the page's remaining videos are already queued as siblings", ErrorCode::NoStreamFound.as_str())
            .context(ErrorCode::NoStreamFound.as_str()))
    } else if known_unsupported {
        repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Extract),
            "yt-dlp has no extractor for this page; going straight to the browser",
        )?;
        Err(anyhow::anyhow!("{}: yt-dlp has no extractor for the page", ErrorCode::UnsupportedUrl.as_str())
            .context(ErrorCode::UnsupportedUrl.as_str()))
    } else {
        engine
            .process_job_mode(
                job.clone(),
                owner,
                page_referer.as_deref(),
                None,
                None,
                Some(cancel.clone()),
                omni_broadcast::pipeline::DeliveryMode::Deliver,
            )
            .await
    };
    Ok(Some(YtdlpAttempt {
        result,
        page_referer,
        scan_siblings,
        // Nothing is left to assess or repair into: the page's videos are queued, or are the site's furniture.
        siblings_hold_the_rest: held_by_siblings,
        only_furniture,
        furniture_urls: furniture.iter().map(|f| f.url.clone()).collect(),
        login_site,
        login_guard,
    }))
}

/// The site or yt-dlp's page scan named a video for this job: the API
/// resolved it (a referer is kept), or the scan found at least one.
/// Whether `video` is on another registrable domain than `article`.
fn other_site(article: &str, video: &str) -> bool {
    match (omni_core::urlnorm::registrable_domain(article), omni_core::urlnorm::registrable_domain(video)) {
        (Some(a), Some(v)) => a != v,
        _ => false,
    }
}

fn site_named_video(api_or_sibling: bool, scanned_videos: usize) -> bool {
    api_or_sibling || scanned_videos >= 1
}

/// The passive verdict with "the site itself named a video" added as a strong
/// signal: such a job is never reported as a page without video.
fn with_site_hint(passive: Option<Evidence>, named_by_site: bool) -> Option<Evidence> {
    if !named_by_site {
        return passive;
    }
    let hint = Evidence { class: EvidenceClass::Strong, signals: vec!["named-by-site".into()], diagnosed: false };
    Some(match passive {
        Some(p) => omni_browser::evidence::combine(&hint, &p),
        None => hint,
    })
}

/// Judge a web page that ended without a video from what the browser saw, run
/// the diagnostic second look when that is allowed and needed, and note the
/// verdict on the job. Returns the code to finish with, a replacement cause
/// line, and the snapshot to keep as evidence (the diagnostic one when there
/// is one). Never fails: whatever goes wrong leaves today's outcome.
async fn assess_failed_page(
    repo: &Repository,
    owner: &str,
    job_id: i64,
    cancel: &CancellationToken,
    page: FailedPage<'_>,
) -> (ErrorCode, Option<String>, Option<omni_browser::PageSnapshot>) {
    let FailedPage { url, mode, snapshot, sniffed_streams, original, named_by_site } = page;
    let is_ad = |u: &str| StreamSniffer::is_ad_or_tracking(u);
    let mut snapshot = snapshot;
    let passive = with_site_hint(
        snapshot.as_ref().map(|snap| omni_browser::evidence::classify(snap, sniffed_streams, None, is_ad)),
        named_by_site,
    );
    let mut verdict = passive.clone();

    if should_diagnose(mode, passive.as_ref().map(|e| e.class), false)
        && !cancel.is_cancelled()
        && still_ours(repo, job_id, owner)
    {
        info!("Job #{job_id}: the page showed no video; looking at it once more with the diagnostic run");
        // Dropping the run closes its browser.
        let ran = tokio::select! {
            r = StreamSniffer::diagnose(url, 30) => Some(r),
            _ = cancel.cancelled() => None,
        };
        let Some((diag_snap, diag)) = ran else {
            return (original, None, snapshot);
        };
        if cancel.is_cancelled() || !still_ours(repo, job_id, owner) {
            return (original, None, snapshot);
        }
        let looked = omni_browser::evidence::classify(&diag_snap, sniffed_streams, Some(&diag), is_ad);
        verdict = Some(match &passive {
            Some(p) => omni_browser::evidence::combine(p, &looked),
            None => looked,
        });
        // A run that was blocked or cut short leaves the first look as the evidence.
        if diag.complete || snapshot.is_none() {
            snapshot = Some(diag_snap);
        }
    }

    let Some(ev) = verdict else {
        return (original, None, snapshot);
    };
    if cancel.is_cancelled() || !still_ours(repo, job_id, owner) {
        return (original, None, snapshot);
    }
    let line = format!("Evidence: {} — {}", ev.class.as_str(), ev.signals.join(", "));
    if let Err(e) = repo.record_event(job_id, "INFO", Some(JobStage::Extract), &line) {
        warn!("Job #{job_id}: could not note the page evidence verdict: {e:#}");
    }
    let (code, message) = evidence_code(original, &ev);
    (code, message, snapshot)
}

/// Largest evidence file written for one job.
const EVIDENCE_MAX_BYTES: usize = 512 * 1024;

/// Write the page evidence of a failed job to `{dir}/{job_id}.json` and note
/// it in the database. A failure here is a warning only: it never changes the
/// job's outcome.
async fn save_page_evidence(
    repo: &Repository,
    dir: &std::path::Path,
    job_id: i64,
    snapshot: &omni_browser::PageSnapshot,
) {
    let path = dir.join(format!("{job_id}.json"));
    let json = snapshot.to_capped_json(EVIDENCE_MAX_BYTES);
    let bytes = json.len() as i64;
    let (dir, file) = (dir.to_path_buf(), path.clone());
    let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&file, json)
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return warn!("Job #{job_id}: page evidence not written: {e}"),
        Err(e) => return warn!("Job #{job_id}: page evidence not written: {e}"),
    }
    if let Err(e) = repo.record_evidence(job_id, &path.to_string_lossy(), bytes) {
        warn!("Job #{job_id}: page evidence not recorded: {e:#}");
    }
}

/// What is known about the article's other videos before they are queued
/// (P3.14): where on the page each sat and whether it was a contextual
/// playlist (from the sniffer; nothing for yt-dlp's own page scan), and the
/// length of each one that would be queued, asked of yt-dlp without
/// downloading. At most `MAX_AUTO_SIBLINGS` probes plus one for each sibling
/// held back (its replacement is asked too), 15 s each, one after another. A length that cannot be had stays unknown and plans as before
/// (queued). Nothing is probed when the journalist asked for a count: they
/// chose those videos.
async fn article_facts(
    engine: &BroadcastEngine,
    job: &omni_core::models::Job,
    primary: &str,
    streams: &[String],
    meta: &std::collections::HashMap<String, omni_browser::sniffer::StreamMeta>,
    cancel: &CancellationToken,
) -> omni_broadcast::article::Facts {
    use omni_broadcast::article::{facts_from_placement, probe_siblings, Probed};
    let mut facts = facts_from_placement(
        streams.iter().filter_map(|u| meta.get(u).map(|m| (u.as_str(), m.in_article, m.contextual))),
    );
    for u in streams {
        if meta.get(u).is_some_and(|m| m.link) {
            if let Some(f) = facts.get_mut(&omni_broadcast::article::facts_key(u)) {
                f.link = true;
            }
        }
    }
    let count = job.max_videos.filter(|n| *n > 0).map(|n| n as usize);
    probe_siblings(&job.url, primary, streams, &job.index_str, count, &mut facts, |url| async move {
        let info = tokio::select! {
            i = engine.video_length(&url) => i,
            _ = cancel.cancelled() => None,
        };
        info.map(|i| Probed { duration_secs: i.duration_secs, live: i.live })
    })
    .await;
    facts
}

/// The candidates of yt-dlp's page scan for [`omni_broadcast::downloader::choose_primary`],
/// in page order, and the meta that marks the glomex contextual lists (`cl-…`)
/// among them. A contextual list is held: never the job's video while another
/// is on the page. Nothing else is known to be held here (P3.14).
fn page_scan_cands(
    videos: &[String],
) -> (Vec<omni_broadcast::downloader::Cand>, std::collections::HashMap<String, omni_browser::sniffer::StreamMeta>) {
    let contextual = |u: &str| omni_browser::glomex::parse_embed(u).is_some_and(|e| e.is_contextual_list());
    let cands = videos
        .iter()
        .map(|u| omni_broadcast::downloader::Cand {
            url: u.clone(),
            held: contextual(u),
            probe_able: omni_broadcast::downloader::is_video_platform(u),
        })
        .collect();
    let meta = videos
        .iter()
        .filter(|u| contextual(u))
        .map(|u| (u.clone(), omni_browser::sniffer::StreamMeta { in_article: None, contextual: true, link: false }))
        .collect();
    (cands, meta)
}

/// `streams` without the social posts yt-dlp says have no video, in page
/// order (P3.13). YouTube is not asked (it has no photo posts) and raw
/// streams cannot be; a post that cannot be checked (network, login) stays.
/// About a second per post with the folder build of yt-dlp.
async fn without_videoless_posts(
    repo: &Repository,
    engine: &BroadcastEngine,
    job_id: i64,
    streams: &[String],
    cancel: &CancellationToken,
) -> Vec<String> {
    use omni_broadcast::downloader::{is_video_platform, is_youtube};
    let mut kept = Vec::with_capacity(streams.len());
    let mut seen = std::collections::HashSet::new();
    for (i, url) in streams.iter().enumerate() {
        // One post under two addresses (".../visegrad24/status/N" and
        // ".../i/status/N") is asked about, and kept, once.
        if !seen.insert(omni_core::urlnorm::normalize(url)) {
            continue;
        }
        let ask = i < 12 && is_video_platform(url) && !is_youtube(url);
        let has_video = if ask {
            // Dropping the future kills yt-dlp (run_capture sets kill_on_drop).
            tokio::select! {
                r = engine.post_has_video(url) => r,
                _ = cancel.cancelled() => break,
            }
        } else {
            None
        };
        if ask && has_video == Some(false) {
            info!("Job #{job_id}: {url} has no video; not a candidate");
            let _ = repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Embedded post without a video, skipped: {url}"));
            continue;
        }
        kept.push(url.clone());
    }
    kept
}

/// The job's failure once the sniffer has also come back empty-handed.
///
/// A page the browser opened and found no video on is reported as that
/// (NO_STREAM_FOUND): yt-dlp's "Unsupported URL" for a news article only
/// means it has no extractor for the portal, which is why the sniffer ran,
/// and told MCR to "try a direct video link" for an article that has none.
/// A browser that never got that far leaves yt-dlp's failure standing.
fn after_failed_sniff(previous: anyhow::Error, sniff_err: anyhow::Error) -> anyhow::Error {
    match sniff_err.downcast_ref::<omni_browser::BrowserError>() {
        Some(omni_browser::BrowserError::NoStreamFound(_)) => {
            anyhow::anyhow!("{}: no video found in the page", ErrorCode::NoStreamFound.as_str())
                .context(ErrorCode::NoStreamFound.as_str())
        }
        _ => previous,
    }
}

/// Whether the engine stopped because this worker no longer holds the job
/// (P1.14). Nothing more may be written for it: `finish` and `requeue_after`
/// would refuse, but the events and sniffing would not.
fn lost_lease<T>(result: &anyhow::Result<T>) -> bool {
    matches!(result, Err(e) if e.downcast_ref::<omni_broadcast::pipeline::LeaseLost>().is_some())
}

/// The one readable line for a failure's `error_message` (P7.15): the root
/// cause, without the code strings the pipeline wraps around it.
/// The error chain for the job's DEBUG diagnostic, as `{e:#}` would print it,
/// minus a bare code layer whose next layer already starts with that code: the
/// pipeline adds `.context(code)` to a download error that names its code itself,
/// which printed "LIVE_STREAM: LIVE_STREAM: …" (2026-10-19).
fn diagnostic_chain(e: &anyhow::Error) -> String {
    let parts: Vec<String> = e.chain().map(|c| c.to_string()).collect();
    let mut kept: Vec<&str> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let repeated = ErrorCode::from_code(part).is_some()
            && parts.get(i + 1).is_some_and(|next| next.starts_with(part.as_str()));
        if !repeated {
            kept.push(part);
        }
    }
    kept.join(": ")
}

fn readable_cause(e: &anyhow::Error) -> String {
    const CAP: usize = 300;
    const TAIL: &str = ". stderr tail: ";
    let outer = format!("{e}");
    let chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
    let Some(ri) = chain.iter().rposition(|s| ErrorCode::from_code(s).is_none()) else { return outer };
    let root = &chain[ri];

    let strip = |mut text: &str| -> String {
        loop {
            let before = text;
            if let Some(rest) = text.strip_prefix("ERROR:") {
                text = rest.trim_start();
            }
            for code in ErrorCode::ALL {
                if let Some(rest) = text.strip_prefix(code.as_str()).and_then(|r| r.strip_prefix(':')) {
                    text = rest.trim_start();
                }
            }
            if text == before {
                return text.trim().to_string();
            }
        }
    };

    let text = if let Some((header, tail)) = root.split_once(TAIL) {
        // A tool that failed (omni_core::process): keep "exited / timed out",
        // add the tail's FIRST line that names an error. FFmpeg 7+ follows
        // the cause ("Error while opening encoder - maybe incorrect
        // parameters…") with generic thread-teardown lines that also say
        // "error"; those name nothing (seen with the installed FFmpeg 9).
        const GENERIC: [&str; 5] = [
            "Conversion failed!",
            "Task finished with error code",
            "Terminating thread with return code",
            "Error sending frames to consumers",
            "Nothing was written into output file",
        ];
        let header = strip(header);
        let hit = tail
            .lines()
            .map(|l| without_log_prefixes(l.trim()))
            .find(|l| l.to_ascii_lowercase().contains("error") && !GENERIC.iter().any(|g| l.contains(g)));
        match hit {
            Some(l) => format!("{header}: {}", strip(l)),
            None => header,
        }
    } else {
        // A stderr tail: the ERROR line if there is one, else the last line with text.
        let lines: Vec<&str> = root.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let line = lines.iter().rev().find(|l| l.contains("ERROR")).or(lines.last()).copied().unwrap_or("");
        let line = strip(line);
        // A bare OS or serde error says little alone; keep what was being done.
        // The OS error goes first: a parent quoting two long paths
        // ("Failed copying … to …") would otherwise push it past the cap.
        let parent = ri.checked_sub(1).map(|i| chain[i].as_str());
        match parent {
            Some(p) if lines.len() <= 1 && ErrorCode::from_code(p).is_none() && !p.trim().is_empty() && !line.is_empty() => {
                format!("{line} ({})", p.trim())
            }
            _ => line,
        }
    };
    if text.is_empty() {
        return outer;
    }
    if text.chars().count() > CAP {
        let cut: String = text.chars().take(CAP).collect();
        return format!("{}…", cut.trim_end());
    }
    text
}

/// "[vost#0:0/mpeg2video @ 000001] [enc:mpeg2video @ 000002] Error …" ->
/// "Error …": FFmpeg's per-component prefixes carry only addresses.
fn without_log_prefixes(mut line: &str) -> &str {
    while line.starts_with('[') {
        match line.find("] ") {
            Some(end) => line = line[end + 2..].trim_start(),
            None => break,
        }
    }
    line
}

/// Recover the error code the pipeline attached to a failure.
///
/// The pipeline wraps failures with `.context(code.as_str())`, so the code is in
/// the anyhow chain rather than parsed back out of a message. Anything
/// unrecognised goes to a human rather than being retried on a guess.
fn classify_pipeline_error(e: &anyhow::Error) -> ErrorCode {
    let text = format!("{e:#}");
    for code in [
        ErrorCode::ManualDownload,
        ErrorCode::HeldFileChanged,
        ErrorCode::ComplianceFailed,
        ErrorCode::ProbeFailed,
        ErrorCode::LoginRequired,
        ErrorCode::GeoBlocked,
        ErrorCode::PrivateOrRemoved,
        ErrorCode::PageNotFound,
        ErrorCode::LiveStream,
        ErrorCode::Http403,
        ErrorCode::UnsupportedUrl,
        ErrorCode::NoStreamFound,
        ErrorCode::DownloadTimeout,
        ErrorCode::TranscodeTimeout,
        ErrorCode::RewrapTimeout,
        ErrorCode::DeliveryFailed,
        ErrorCode::LowDisk,
        ErrorCode::Network,
    ] {
        if text.contains(code.as_str()) {
            return code;
        }
    }
    ErrorCode::PipelineFailed
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
) -> omni_core::scheduler::TaskOutcome {
    use omni_core::scheduler::TaskOutcome;

    info!("Running scheduled nightly yt-dlp update check...");

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
        return TaskOutcome::Ok;
    }

    // Up to ten minutes for in-flight downloads. Longer than any sane clip and
    // shorter than the gap to the next bulletin.
    if !gate.pause_and_drain(Duration::from_secs(600)).await {
        gate.resume();
        warn!(
            "yt-dlp update postponed: {} download(s) still running after 10 minutes. The \
             verified build stays staged and will be applied on the next attempt.",
            gate.active()
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
            gate.resume();
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
    let verdict = verify_ytdl_update(dep_mgr, &installed, repo).await;
    gate.resume();
    match verdict {
        UpdateVerdict::Kept { checked } => {
            info!("Nightly yt-dlp updated to {:?} ({checked} self-check link(s) still work)", installed);
            let _ = repo.log_audit(
                "INFO",
                "SYSTEM",
                &format!(
                    "yt-dlp updated ({} channel, sha256 {}); {checked} self-check link(s) still work; previous build kept for rollback",
                    channel,
                    &staged.sha256[..16]
                ),
            );
            // A new downloader may resolve what the model once could not be asked about.
            if let Err(e) = repo.close_all_llm_breakers() {
                warn!("The repair model breakers could not be closed after the update: {e:#}");
            }
            TaskOutcome::Ok
        }
        UpdateVerdict::RolledBack { broken } => {
            let _ = std::fs::write(&rejected_marker, &staged.sha256);
            warn!("yt-dlp update rolled back: it broke {}", broken.join(", "));
            let _ = repo.log_audit(
                "ERROR",
                "SYSTEM",
                &format!(
                    "yt-dlp update rolled back automatically: the new build could not get {} while the previous one could. The previous build is in use; the next release will be tried.",
                    broken.join(", ")
                ),
            );
            TaskOutcome::Failed
        }
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
) -> omni_core::scheduler::TaskOutcome {
    use omni_core::scheduler::TaskOutcome;
    let Some(installed) = dep_mgr.deno_version() else {
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
    if !gate.pause_and_drain(Duration::from_secs(600)).await {
        gate.resume();
        return TaskOutcome::Skipped;
    }
    let result = dep_mgr.install_deno().await;
    let outcome = match result {
        Err(e) => {
            warn!("Deno update not installed: {e:#}");
            let _ = repo.log_audit("WARN", "SYSTEM", &format!("Deno update skipped: {e}"));
            TaskOutcome::Failed
        }
        Ok(new_version) => {
            let worked: Vec<omni_core::models::SelfcheckLink> = repo
                .list_selfcheck_links()
                .unwrap_or_default()
                .into_iter()
                .filter(|l| l.last_ok == Some(true))
                .collect();
            let ytdl = dep_mgr.ytdl_live();
            let broken: Vec<String> = selfcheck::check_and_record(repo, &ytdl, &worked)
                .await
                .into_iter()
                .filter(|(_, o)| !o.ok)
                .map(|(l, _)| l.label)
                .collect();
            if broken.is_empty() {
                let _ = repo.log_audit("INFO", "SYSTEM", &format!("Deno updated {installed} → {new_version}"));
                TaskOutcome::Ok
            } else {
                let _ = dep_mgr.rollback_deno();
                let _ = repo.log_audit(
                    "ERROR",
                    "SYSTEM",
                    &format!("Deno {new_version} rolled back to {installed}: {} stopped working with it", broken.join(", ")),
                );
                TaskOutcome::Failed
            }
        }
    };
    gate.resume();
    if let Some(v) = dep_mgr.deno_version() {
        health.set(omni_core::health::checks::DENO, omni_core::health::Check::ok(format!("Deno {v}")));
    }
    outcome
}

enum UpdateVerdict {
    Kept { checked: usize },
    RolledBack { broken: Vec<String> },
}

async fn verify_ytdl_update(dep_mgr: &DependencyManager, live: &std::path::Path, repo: &Repository) -> UpdateVerdict {
    let worked: Vec<omni_core::models::SelfcheckLink> = repo
        .list_selfcheck_links()
        .unwrap_or_default()
        .into_iter()
        .filter(|l| l.last_ok == Some(true))
        .collect();
    let results = selfcheck::check_and_record(repo, live, &worked).await;
    let failed: Vec<omni_core::models::SelfcheckLink> =
        results.into_iter().filter(|(_, o)| !o.ok).map(|(l, _)| l).collect();
    if failed.is_empty() {
        return UpdateVerdict::Kept { checked: worked.len() };
    }

    if let Err(e) = dep_mgr.rollback_ytdl() {
        warn!("yt-dlp verification: could not put the previous build back to compare: {e:#}");
        return UpdateVerdict::Kept { checked: worked.len() };
    }
    let with_old = selfcheck::check_and_record(repo, live, &failed).await;
    let broken: Vec<String> = with_old.iter().filter(|(_, o)| o.ok).map(|(l, _)| l.label.clone()).collect();
    if broken.is_empty() {
        // Both builds fail those links: the sites changed or the network is
        // down. The newer build is the better bet for a fix; put it back.
        if let Err(e) = dep_mgr.rollback_ytdl() {
            warn!("yt-dlp verification: could not reinstate the new build: {e:#}");
        }
        // Record what the build that stays in place sees.
        selfcheck::check_and_record(repo, live, &failed).await;
        return UpdateVerdict::Kept { checked: worked.len() };
    }
    UpdateVerdict::RolledBack { broken }
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
}

impl omni_web::state::CookieTester for CookieJarTester {
    fn test<'a>(&'a self, domain: &'a str, url: &'a str) -> omni_web::state::CookieTestFuture<'a> {
        Box::pin(async move {
            let ytdl = DependencyManager::new(&self.bin_dir).ytdl_live();
            let t = omni_broadcast::cookie_file::test_jar(&ytdl, &self.temp_path, &self.repo, domain, url).await?;
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
                let ytdl = nightly_ytdl_update(&dep_mgr, &ctx.ytdl_channel, &ctx.gate, repo).await;
                let deno = nightly_deno_update(&dep_mgr, &ctx.gate, repo, &ctx.health).await;
                (ytdl, deno)
            })
            .await;
            selfcheck::refresh_health(repo, &ctx.health);
            // The worse of the two is what the maintenance panel shows.
            Ok(match (ytdl, deno) {
                (TaskOutcome::Failed, _) | (_, TaskOutcome::Failed) => TaskOutcome::Failed,
                (TaskOutcome::Ok, _) | (_, TaskOutcome::Ok) => TaskOutcome::Ok,
                _ => TaskOutcome::Skipped,
            })
        }

        "selfcheck" => {
            let ytdl = DependencyManager::new(&ctx.bin_dir).ytdl_live();
            let failing = selfcheck::run(repo, &ytdl, &ctx.health).await?;
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
            let (mut tested, mut failing) = (0usize, Vec::<String>::new());
            for jar in &jars {
                let Some(url) = omni_core::cookies::sample_url_for(&jar.domain, jar.last_sample_url.as_deref()) else { continue };
                tested += 1;
                match omni_broadcast::cookie_file::test_jar(&ytdl, &ctx.temp_path, repo, &jar.domain, &url).await {
                    Ok(t) if t.ok => {}
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
    async fn the_page_scan_never_picks_a_contextual_list_or_a_live_video() {
        use omni_broadcast::downloader::{choose_primary, Choice, Probe};
        let cl = "https://player.glomex.com/integration/1/iframe-player.html?integrationId=i1&playlistId=cl-abc".to_string();
        let live = "https://www.youtube.com/watch?v=liveliveli1".to_string();
        let x = "https://x.com/i/status/2".to_string();
        let run = |videos: Vec<String>, live: String| async move {
            let (cands, meta) = page_scan_cands(&videos);
            let l = live.clone();
            (choose_primary(&cands, 3, move |u| {
                let is = u == l;
                async move { if is { Probe::Live } else { Probe::Ok } }
            })
            .await, meta)
        };
        // [cl-, LIVE youtube, x post] -> the x post.
        let (c, meta) = run(vec![cl.clone(), live.clone(), x.clone()], live.clone()).await;
        assert!(matches!(c, Choice::Primary { ref url, .. } if *url == x), "{c:?}");
        assert!(meta[&cl].contextual && meta.len() == 1);
        // [LIVE youtube, cl-] -> all live, not the cl- list.
        let (c, _) = run(vec![live.clone(), cl.clone()], live.clone()).await;
        assert!(matches!(c, Choice::AllLive { .. }), "{c:?}");
        // Nothing live: the first non-contextual, as before; a concrete clip keeps its place.
        let clip = "https://player.glomex.com/integration/1/iframe-player.html?integrationId=i1&playlistId=v-abc".to_string();
        let (c, meta) = run(vec![clip.clone(), x.clone()], String::new()).await;
        assert!(matches!(c, Choice::Primary { ref url, .. } if *url == clip), "{c:?}");
        assert!(meta.is_empty());
        let (c, _) = run(vec![cl.clone(), x.clone()], String::new()).await;
        assert!(matches!(c, Choice::Primary { ref url, .. } if *url == x), "{c:?}");
    }

    #[test]
    fn a_single_video_on_another_site_names_that_site_and_leaves_the_job_alone() {
        assert!(other_site("https://www.news.example/story/1", "https://x.com/a/status/1"));
        assert!(!other_site("https://www.news.example/story/1", "https://cdn.news.example/v/1"));
        // The hint is keyed on the embed's site while the job's own address stays the article.
        let dir = std::env::temp_dir().join(format!("omni-f6-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repository::new(dir.join("omni.db")).unwrap();
        let article = "https://www.news.example/story/1";
        let embed = Some("https://x.com/a/status/1".to_string());
        assert_eq!(
            omni_broadcast::errors::hint_for_job(ErrorCode::LoginRequired, embed.as_deref().unwrap_or(article), &repo),
            omni_broadcast::errors::login_hint_el("x.com", false)
        );
        assert_eq!(
            omni_broadcast::errors::hint_for_job(ErrorCode::LoginRequired, None::<String>.as_deref().unwrap_or(article), &repo),
            ErrorCode::LoginRequired.hint_el()
        );
        drop(repo);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A scratch database that removes itself.
    struct ScratchDb(std::path::PathBuf);
    impl ScratchDb {
        fn new() -> (Self, Repository) {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("omni-main-test-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let repo = Repository::new(dir.join("omni.db")).unwrap();
            (Self(dir), repo)
        }
    }
    impl Drop for ScratchDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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

    #[test]
    fn finish_held_parks_the_job_for_its_owner_and_nobody_else() {
        let (_db, repo) = ScratchDb::new();
        let id = repo
            .enqueue(
                &omni_core::models::NewJob::new("https://example.gr/h", "1_ANNA_HELD", "ANNA"),
                omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
            )
            .unwrap()
            .job_id();
        repo.lease_job("w1", 180).unwrap().expect("leased");

        assert!(!finish_held(&repo, id, "w2").unwrap(), "another worker must not park the job");
        assert_eq!(repo.get_job(id).unwrap().unwrap().status, JobStatus::Running);

        assert!(finish_held(&repo, id, "w1").unwrap());
        let job = repo.get_job(id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::AwaitingApproval);
        assert_eq!(job.lease_owner, None);
        assert!(!finish_held(&repo, id, "w1").unwrap(), "the lease is gone once parked");
        assert!(repo.lease_job("w3", 180).unwrap().is_none(), "a parked job is not leased");
    }

    /// P10.5: a changed held file is a terminal, named failure, never "unexpected".
    #[test]
    fn a_changed_held_file_classifies_as_its_own_terminal_code() {
        let e = anyhow::anyhow!("{}: the video changed", ErrorCode::HeldFileChanged.as_str())
            .context(ErrorCode::HeldFileChanged.as_str());
        let code = classify_pipeline_error(&e);
        assert_eq!(code, ErrorCode::HeldFileChanged);
        assert!(!code.is_retryable(), "a file that is not the approved one is never retried into delivery");
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

    /// 2026-10-19: a live download's DEBUG line read "LIVE_STREAM: LIVE_STREAM: …".
    #[test]
    fn the_diagnostic_chain_names_a_code_once_and_keeps_every_other_layer() {
        let e = anyhow::anyhow!("LIVE_STREAM: the video is live or not yet started; yt-dlp skipped it").context("LIVE_STREAM");
        assert_eq!(format!("{e:#}"), "LIVE_STREAM: LIVE_STREAM: the video is live or not yet started; yt-dlp skipped it");
        assert_eq!(diagnostic_chain(&e), "LIVE_STREAM: the video is live or not yet started; yt-dlp skipped it");
        // A code layer over a different message, and plain layers, all stay.
        let e = anyhow::anyhow!("exited with 1. stderr tail: boom").context("transcode failed").context("TRANSCODE_FAILED");
        assert_eq!(diagnostic_chain(&e), format!("{e:#}"));
    }

    /// Job #20 (2026-10-08): the card said PIPELINE_FAILED; the reason was only
    /// in the timeline.
    #[test]
    fn readable_cause_is_the_root_of_the_chain() {
        let e = anyhow::anyhow!("ERROR: [twitter] 2106778297484939750: No video could be found in this tweet")
            .context("PIPELINE_FAILED");
        assert_eq!(readable_cause(&e), "[twitter] 2106778297484939750: No video could be found in this tweet");
    }

    #[test]
    fn readable_cause_prefers_the_error_line_of_a_stderr_tail() {
        let e = anyhow::anyhow!("WARNING: something minor\nERROR: [generic] Unable to download webpage: HTTP Error 404\n\n")
            .context("PAGE_NOT_FOUND");
        assert_eq!(readable_cause(&e), "[generic] Unable to download webpage: HTTP Error 404");
        let plain = anyhow::anyhow!("first\nlast line\n\n").context("NETWORK");
        assert_eq!(readable_cause(&plain), "last line");
    }

    #[test]
    fn readable_cause_of_a_bare_code_is_the_code() {
        let e = anyhow::anyhow!("PIPELINE_FAILED");
        assert_eq!(readable_cause(&e), "PIPELINE_FAILED");
        let repeated = anyhow::anyhow!("PIPELINE_FAILED: PIPELINE_FAILED: boom").context("PIPELINE_FAILED");
        assert_eq!(readable_cause(&repeated), "boom");
    }

    #[test]
    fn readable_cause_keeps_the_header_of_a_tool_failure() {
        let ff = anyhow::anyhow!(
            "ffmpeg exited with code Some(1). stderr tail: frame=0 fps=0.0\n[mpeg2video @ 000001] Error initializing output stream\nError while opening encoder for output stream #0:0 - maybe incorrect parameters\nConversion failed!\n"
        )
        .context("PIPELINE_FAILED");
        let s = readable_cause(&ff);
        assert!(s.starts_with("ffmpeg exited with code Some(1)"), "{s}");
        assert!(s.ends_with(": Error initializing output stream"), "the first error line, without its [… @ addr] prefix: {s}");
        assert!(!s.contains("Conversion failed"), "{s}");

        // The tail the installed FFmpeg 9 writes for an encoder that will not
        // open (VBV buffer too small): the cause comes first, then lines
        // that also say "error" but name nothing.
        let ff9 = anyhow::anyhow!(concat!(
            "ffmpeg exited with code Some(1). stderr tail: ",
            "[mpeg2video @ 0000029b555468c0] VBV buffer too small for bitrate\n",
            "[vost#0:0/mpeg2video @ 0000029b55546680] [enc:mpeg2video @ 0000029b4afd5540] Error while opening encoder - maybe incorrect parameters such as bit_rate, rate, width or height.\n",
            "[vf#0:0 @ 0000029b55549840] Error sending frames to consumers: Invalid argument\n",
            "[vf#0:0 @ 0000029b55549840] Task finished with error code: -22 (Invalid argument)\n",
            "[vost#0:0/mpeg2video @ 0000029b55546680] [enc:mpeg2video @ 0000029b4afd5540] Could not open encoder before EOF\n",
            "[vf#0:0 @ 0000029b55549840] Terminating thread with return code -22 (Invalid argument)\n",
            "[vost#0:0/mpeg2video @ 0000029b55546680] Task finished with error code: -22 (Invalid argument)\n",
            "[vost#0:0/mpeg2video @ 0000029b55546680] Terminating thread with return code -22 (Invalid argument)\n",
            "[out#0/null @ 0000029b55544780] Nothing was written into output file, because at least one of its streams received no packets.\n",
            "frame=    0 fps=0.0 q=0.0 Lsize=       0KiB time=N/A bitrate=N/A speed=N/A elapsed=0:00:00.02\n",
            "Conversion failed!\n",
        ))
        .context("TRANSCODE_FAILED");
        assert_eq!(
            readable_cause(&ff9),
            "ffmpeg exited with code Some(1): Error while opening encoder - maybe incorrect parameters such as bit_rate, rate, width or height."
        );

        let timeout = anyhow::anyhow!("ffmpeg timed out after 3600s; process tree killed. stderr tail: frame=10\nspeed=0.1x\n")
            .context("TRANSCODE_TIMEOUT");
        assert_eq!(readable_cause(&timeout), "ffmpeg timed out after 3600s; process tree killed");

        let ytdlp = anyhow::anyhow!("yt-dlp exited with code Some(1). stderr tail: WARNING: x\nERROR: [twitter] 1: No video could be found\n")
            .context("PIPELINE_FAILED");
        let s = readable_cause(&ytdlp);
        assert!(s.starts_with("yt-dlp exited with code Some(1)"), "{s}");
        assert!(s.ends_with("[twitter] 1: No video could be found"), "{s}");
    }

    #[test]
    fn readable_cause_keeps_the_parent_of_a_bare_os_error() {
        let e = anyhow::anyhow!("The network path was not found. (os error 53)")
            .context("Failed creating the watchfolder")
            .context("DELIVERY_FAILED");
        let s = readable_cause(&e);
        assert!(s.contains("Failed creating the watchfolder") && s.contains("os error 53"), "{s}");
        // Two long quoted paths in the parent must not push the OS error
        // itself past the cap.
        let long = format!("Failed copying \"{}\" to \"{}\"", "C:\\OmniIngest\\temp\\jobs\\42\\x".repeat(6), "\\\\dalet\\watch\\y".repeat(8));
        let e = anyhow::anyhow!("Access is denied. (os error 5)").context(long).context("DELIVERY_FAILED");
        assert!(readable_cause(&e).starts_with("Access is denied. (os error 5)"));
        // A code above the root is not a parent worth quoting.
        let coded = anyhow::anyhow!("boom").context("PIPELINE_FAILED");
        assert_eq!(readable_cause(&coded), "boom");
    }

    #[test]
    fn readable_cause_is_capped_on_a_char_boundary() {
        let e = anyhow::anyhow!("Σφάλμα λήψης βίντεο ".repeat(50)).context("PIPELINE_FAILED");
        let s = readable_cause(&e);
        assert!(s.chars().count() <= 301, "{}", s.chars().count());
        assert!(s.ends_with('…'));
    }

    /// The engine's lost-lease error is recognised by type; a failure that
    /// merely quotes the code in its text (a URL ending in it) is not.
    #[test]
    fn a_lost_lease_is_recognised_by_type_not_by_text() {
        let lost: anyhow::Result<()> = Err(anyhow::Error::new(omni_broadcast::pipeline::LeaseLost { job_id: 7, stage: JobStage::Rewrap })
            .context(ErrorCode::LeaseExpired.as_str()));
        assert!(lost_lease(&lost));

        let quoting: anyhow::Result<()> = Err(anyhow::anyhow!("ERROR: unable to download https://example.gr/LEASE_EXPIRED")
            .context("LEASE_EXPIRED"));
        assert!(!lost_lease(&quoting));
        assert!(!lost_lease(&Ok(())));
    }

    /// neakriti.gr article without a video (2026-10-05): the card said
    /// UNSUPPORTED_URL, "try a direct video link".
    #[test]
    fn a_page_without_a_video_is_reported_as_no_stream_not_unsupported() {
        let yt_dlp = || anyhow::anyhow!("ERROR: Unsupported URL: https://www.neakriti.gr/x").context(ErrorCode::UnsupportedUrl.as_str());

        let empty = anyhow::Error::from(omni_browser::BrowserError::NoStreamFound("https://www.neakriti.gr/x".into()));
        assert_eq!(classify_pipeline_error(&after_failed_sniff(yt_dlp(), empty)), ErrorCode::NoStreamFound);

        let no_browser = anyhow::Error::from(omni_browser::BrowserError::LaunchFailed("no Chrome".into()));
        assert_eq!(classify_pipeline_error(&after_failed_sniff(yt_dlp(), no_browser)), ErrorCode::UnsupportedUrl);
    }

    /// A mistyped YouTube id (2026-10-07): the card said «Απρόβλεπτο
    /// σφάλμα». The downloader's code must come back out of the chain the
    /// pipeline wraps around it.
    #[test]
    fn an_unavailable_video_keeps_its_code_through_the_pipeline() {
        let line = "ERROR: [youtube] zzzzzzzzzz0: This video is unavailable";
        let code = omni_broadcast::errors::classify_download_error(line);
        let e = anyhow::Error::from(omni_broadcast::downloader::DownloadError { code, message: line.into() })
            .context(code.as_str());
        assert_eq!(classify_pipeline_error(&e), ErrorCode::PrivateOrRemoved);
    }

    fn verdict(class: EvidenceClass, signals: &[&str], diagnosed: bool) -> Evidence {
        Evidence { class, signals: signals.iter().map(|s| s.to_string()).collect(), diagnosed }
    }

    /// The code a page's evidence gives a failed job (P10.2): a "no video"
    /// verdict needs the second look; a weak page keeps what it had.
    #[test]
    fn only_four_codes_count_against_a_site() {
        let counting: Vec<&str> = ErrorCode::ALL
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

    #[test]
    fn evidence_names_the_exact_failure() {
        let orig = ErrorCode::NoStreamFound;
        let (c, m) = evidence_code(orig, &verdict(EvidenceClass::None, &[], false));
        assert_eq!((c, m), (orig, None), "not diagnosed: no claim that the page has no video");
        let (c, m) = evidence_code(orig, &verdict(EvidenceClass::None, &[], true));
        assert_eq!(c, ErrorCode::NoVideoOnPage);
        assert_eq!(m.as_deref(), Some("Δεν βρέθηκε βίντεο ούτε στον δεύτερο έλεγχο"));
        assert_eq!(evidence_code(ErrorCode::Http403, &verdict(EvidenceClass::Weak, &["player-container"], true)), (ErrorCode::Http403, None));
        let (c, m) = evidence_code(orig, &verdict(EvidenceClass::Strong, &["og:video", "embed:jwplayer"], false));
        assert_eq!(c, ErrorCode::VideoUnresolved);
        assert_eq!(m.as_deref(), Some("Βρέθηκε βίντεο στη σελίδα (og:video, embed:jwplayer) αλλά δεν κατέβηκε"));
        let (c, m) = evidence_code(orig, &verdict(EvidenceClass::Protected, &["eme:com.widevine.alpha", "mse:video/mp4"], true));
        assert_eq!(c, ErrorCode::DrmProtected);
        assert_eq!(m.as_deref(), Some("Το βίντεο έχει DRM (com.widevine.alpha)"));
        assert_eq!(evidence_code(orig, &verdict(EvidenceClass::LoginWall, &["http-401 cdn.example"], true)), (ErrorCode::LoginRequired, None));
        assert_eq!(evidence_code(orig, &verdict(EvidenceClass::GeoBlocked, &["http-451 cdn.example"], false)), (ErrorCode::GeoBlocked, None));
        // every result is a final code: nothing here sends a job round again
        for class in [EvidenceClass::Protected, EvidenceClass::LoginWall, EvidenceClass::GeoBlocked, EvidenceClass::Strong] {
            let (c, _) = evidence_code(ErrorCode::UnsupportedUrl, &verdict(class, &[], true));
            assert!(!c.is_retryable(), "{c}");
        }
    }

    #[test]
    fn the_second_look_needs_repair_on_a_page_and_nothing_proven() {
        use EvidenceClass as C;
        for passive in [None, Some(C::Weak), Some(C::None)] {
            assert!(!should_diagnose(RepairMode::Off, passive, false), "off never diagnoses");
            assert!(should_diagnose(RepairMode::Shadow, passive, false));
            assert!(should_diagnose(RepairMode::Hold, passive, false));
            assert!(!should_diagnose(RepairMode::Hold, passive, true), "a platform post is never re-sniffed");
        }
        for passive in [C::Protected, C::LoginWall, C::GeoBlocked, C::Strong] {
            assert!(!should_diagnose(RepairMode::Hold, Some(passive), false), "{passive:?} is already decided");
        }
    }

    /// A job the site itself named a video for can never be "no video on the
    /// page", and needs no second look.
    #[test]
    fn one_scanned_video_counts_as_named_by_the_site() {
        assert!(!site_named_video(false, 0));
        assert!(site_named_video(false, 1), "a single video is enough, not only several");
        assert!(site_named_video(false, 3));
        assert!(site_named_video(true, 0));
    }

    #[test]
    fn a_video_the_site_named_is_never_reported_missing() {
        assert!(with_site_hint(None, false).is_none());
        let named = with_site_hint(None, true).unwrap();
        assert_eq!(named.class, EvidenceClass::Strong);
        assert_eq!(named.signals, vec!["named-by-site"]);
        assert_eq!(evidence_code(ErrorCode::NoStreamFound, &named).0, ErrorCode::VideoUnresolved);
        assert!(!should_diagnose(RepairMode::Hold, Some(named.class), false));
        // over a weak or empty look it wins; over a refusal it does not
        let weak = verdict(EvidenceClass::Weak, &["player-container"], false);
        let both = with_site_hint(Some(weak), true).unwrap();
        assert_eq!(both.class, EvidenceClass::Strong);
        assert!(both.signals.contains(&"player-container".to_string()));
        let drm = with_site_hint(Some(verdict(EvidenceClass::Protected, &["drm:setkeys"], false)), true).unwrap();
        assert_eq!(drm.class, EvidenceClass::Protected);
    }

    /// A job MCR cancelled while the page was judged gets no verdict event and
    /// keeps today's code; the same page for a job still ours gets both.
    #[tokio::test]
    async fn a_cancelled_job_gets_no_evidence_verdict() {
        let db = std::env::temp_dir().join(format!("omni_p102_cancel_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let repo = Repository::new(&db).unwrap();
        let id = repo
            .enqueue(&omni_core::models::NewJob::new("https://news.example/a", "slug-a", "Test Person"), 24)
            .unwrap()
            .job_id();
        let job = repo.lease_job("w1", 60).unwrap().expect("leased");
        assert_eq!(job.id, id);
        let page = || {
            let mut snap = omni_browser::PageSnapshot::new("https://news.example/a");
            snap.meta = vec![("og:video".into(), "https://cdn.example/v.mp4".into())];
            FailedPage {
                url: "https://news.example/a",
                mode: RepairMode::Off,
                snapshot: Some(snap),
                sniffed_streams: &[],
                original: ErrorCode::NoStreamFound,
                named_by_site: false,
            }
        };
        let notes = |repo: &Repository| {
            repo.get_job_events(id, 50).unwrap().iter().filter(|e| e.message.starts_with("Evidence:")).count()
        };

        let cancel = CancellationToken::new();
        cancel.cancel();
        let (code, line, _) = assess_failed_page(&repo, "w1", id, &cancel, page()).await;
        assert_eq!((code, line), (ErrorCode::NoStreamFound, None));
        assert_eq!(notes(&repo), 0, "no verdict for a cancelled job");

        let live = CancellationToken::new();
        let (code, line, _) = assess_failed_page(&repo, "w1", id, &live, page()).await;
        assert_eq!(code, ErrorCode::VideoUnresolved);
        assert!(line.is_some());
        assert_eq!(notes(&repo), 1);

        let (code, _, _) = assess_failed_page(&repo, "someone-else", id, &live, page()).await;
        assert_eq!(code, ErrorCode::NoStreamFound, "a lost lease gets no verdict either");
        assert_eq!(notes(&repo), 1);
        drop(repo);
        let _ = std::fs::remove_file(&db);
    }
}

#[cfg(test)]
mod sniff_fallback_tests {
    use super::{falls_back_to_ytdlp, sniff_result_end, SniffEnd};


    #[test]
    fn a_sniff_that_ended_on_a_platform_page_is_not_taken_but_only_when_the_browser_was_first() {
        let post = Some("https://www.youtube.com/watch?v=abcdefghijk");
        let page = Some("https://www.example-news.gr/article");
        assert_eq!(sniff_result_end(true, true, post), Some(SniffEnd::LandedOnPlatform));
        assert_eq!(sniff_result_end(true, true, page), None);
        assert_eq!(sniff_result_end(true, true, None), None);
        assert_eq!(sniff_result_end(false, true, post), None, "yt-dlp went first for this page");
        assert_eq!(sniff_result_end(true, false, page), Some(SniffEnd::Error));
        assert!(falls_back_to_ytdlp(true, sniff_result_end(true, true, post).unwrap()));
    }

    #[test]
    fn yt_dlp_is_tried_after_the_browser_only_when_it_found_no_usable_video() {
        assert!(falls_back_to_ytdlp(true, SniffEnd::Error));
        assert!(falls_back_to_ytdlp(true, SniffEnd::PostsHaveNoVideo));
        assert!(falls_back_to_ytdlp(true, SniffEnd::LandedOnPlatform));
        assert!(falls_back_to_ytdlp(true, SniffEnd::SiteWide), "yt-dlp's pick may be another video; guard_scan blocks a furniture one");
        for end in [
            SniffEnd::Found,
            SniffEnd::AllLive,
            SniffEnd::AllDead,
            SniffEnd::SiblingsHoldTheRest,
            SniffEnd::Cancelled,
            SniffEnd::LeaseLost,
            SniffEnd::NotRun,
        ] {
            assert!(!falls_back_to_ytdlp(true, end), "{end:?}");
        }
    }

    #[test]
    fn a_page_the_browser_was_not_asked_first_never_falls_back() {
        assert!(!falls_back_to_ytdlp(false, SniffEnd::Error));
        assert!(!falls_back_to_ytdlp(false, SniffEnd::PostsHaveNoVideo));
    }
}

/// Whether a failed job's end is written to its site's health (P10.3): only a finished,
/// uncancelled job of its own page, with a code that says something about the site. A page whose
/// only videos are the site's furniture (P3.16) fails the job but says nothing about the site.
fn site_failure_counts(done: bool, cancelled: bool, is_child: bool, furniture_only: bool, code: &str) -> bool {
    done && !cancelled && !is_child && !furniture_only && omni_core::site_health::counts_against_site(code)
}

#[cfg(test)]
mod site_failure_tests {
    use super::*;

    #[test]
    fn a_furniture_only_ending_is_not_a_site_failure_but_a_real_no_stream_found_is() {
        assert!(site_failure_counts(true, false, false, false, "NO_STREAM_FOUND"), "control: counts as before");
        assert!(!site_failure_counts(true, false, false, true, "NO_STREAM_FOUND"));
    }

    #[test]
    fn the_furniture_ending_leaves_the_stored_site_health_unchanged() {
        let (_d, repo) = crate::site_memory::tests::repo();
        let health = HealthState::new();
        let url = "https://www.news.example/a/story-1";
        // As the end of run_job does it: only what site_failure_counts allows is recorded.
        for furniture_only in [true, false] {
            if site_failure_counts(true, false, false, furniture_only, "NO_STREAM_FOUND") {
                note_site_outcome(&repo, &health, 1, url, SiteOutcome::SiteFailure("NO_STREAM_FOUND".into()));
            }
            let row = repo.get_site("news.example").unwrap();
            if furniture_only {
                assert!(row.is_none(), "nothing recorded for the site");
            } else {
                assert_eq!(row.unwrap().consecutive_fail, 1, "a real NO_STREAM_FOUND still counts");
            }
        }
    }
}

/// Whether a failed job ended for lack of a video only because the page's videos are the site's
/// furniture (P3.16), which says nothing about the site: `pick_refused` is yt-dlp's first pick refused
/// as furniture (the route where yt-dlp goes first), `sniff` how the browser's look ended, and
/// `fallback_refused` whether yt-dlp's pick after the browser was refused (`None`: not made).
fn furniture_only_end(pick_refused: bool, sniff: SniffEnd, fallback_refused: Option<bool>) -> bool {
    match sniff {
        SniffEnd::SiteWide => fallback_refused.unwrap_or(true),
        SniffEnd::Error | SniffEnd::PostsHaveNoVideo | SniffEnd::LandedOnPlatform => fallback_refused.unwrap_or(pick_refused),
        SniffEnd::NotRun => pick_refused,
        SniffEnd::Found
        | SniffEnd::AllLive
        | SniffEnd::AllDead
        | SniffEnd::SiblingsHoldTheRest
        | SniffEnd::Cancelled
        | SniffEnd::LeaseLost => false,
    }
}

#[cfg(test)]
mod furniture_end_tests {
    use super::*;

    #[test]
    fn the_browser_first_route_ends_on_furniture_unless_yt_dlps_own_pick_is_another_video() {
        assert!(furniture_only_end(false, SniffEnd::SiteWide, None), "no pick was made after the browser");
        assert!(furniture_only_end(false, SniffEnd::SiteWide, Some(true)), "the pick was the same promo");
        assert!(!furniture_only_end(false, SniffEnd::SiteWide, Some(false)), "another video that then failed is a real failure");
        assert!(furniture_only_end(false, SniffEnd::Error, Some(true)), "browser found nothing, pick refused");
        assert!(!furniture_only_end(false, SniffEnd::Error, Some(false)));
    }

    #[test]
    fn the_yt_dlp_first_route_keeps_a_refused_pick_through_a_sniff_that_finds_nothing() {
        for end in [SniffEnd::Error, SniffEnd::PostsHaveNoVideo, SniffEnd::SiteWide, SniffEnd::NotRun] {
            assert!(furniture_only_end(true, end, None), "{end:?}");
            assert!(!furniture_only_end(false, end, None) || end == SniffEnd::SiteWide, "{end:?}: no refused pick, a real failure");
        }
    }

    #[test]
    fn a_sniff_that_chose_a_video_or_ended_for_another_reason_is_a_real_ending() {
        for end in [SniffEnd::Found, SniffEnd::AllLive, SniffEnd::AllDead, SniffEnd::SiblingsHoldTheRest, SniffEnd::Cancelled, SniffEnd::LeaseLost] {
            assert!(!furniture_only_end(true, end, Some(true)), "{end:?}");
        }
    }
}
