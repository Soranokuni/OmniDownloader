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

mod selfcheck;

#[derive(Parser)]
#[command(name = "omni-ingest")]
#[command(author = "Alex Fountas <afountas@cretetv.gr>")]
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
    info!("Install root: {:?}", paths.root);

    // The adblock cache must be absolute before any sniff can run.
    omni_browser::UnifiedAdBlocker::init(paths.data.join("adblock"));

    match cli.command {
        None | Some(Commands::Run) => {
            run_daemon(&paths, None).await?;
        }
        Some(Commands::RunService) => {
            let paths = paths.clone();
            omni_service::run_service(move |shutdown_rx| {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build Tokio runtime for Windows Service");
                rt.block_on(async {
                    if let Err(e) = run_daemon(&paths, Some(shutdown_rx)).await {
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
    tokio::fs::create_dir_all(&watchfolder_path).await?;
    tokio::fs::create_dir_all(&bin_dir).await?;

    info!("Initializing SQLite database at: {:?}", db_path);
    let repo = Repository::new(&db_path)?;
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
    .with_encoder_slots(config.max_concurrent_transcodes.clamp(1, 8)));

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

    // Tools that start are not tools that make the file: an FFmpeg 9 build
    // answered `-version` and then rejected an output option, so every job
    // failed at transcode under a green "tools" check. One real 2 s job —
    // transcode, RDD9 rewrap, compliance gate — proves the chain.
    let encoder_ok = Arc::new(std::sync::atomic::AtomicBool::new(false));
    match omni_broadcast::selftest::encoder_self_test(&st_ffmpeg, &st_ffprobe, &st_bmx, &temp_path).await {
        Ok(report) => {
            encoder_ok.store(true, std::sync::atomic::Ordering::SeqCst);
            health.set(
                omni_core::health::checks::ENCODER,
                omni_core::health::Check::ok(format!(
                    "test clip transcoded, rewrapped and verified in {:.1} s",
                    report.elapsed.as_secs_f64()
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
    let web_state = AppState::new(repo.clone(), config.clone(), config_path.to_path_buf())
        .with_secret_store(secret_store.clone())
        .with_health(health.clone())
        .with_llm(live_llm.clone())
        .with_busy_jobs(busy_jobs.clone());
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
            ytdl_channel: config.ytdl_channel.clone(),
            ytdl_enabled: config.ytdl_auto_update_nightly,
            adblock_enabled: config.adblock_auto_update_nightly,
            gate: update_gate.clone(),
            retention_days: config.retention_days.max(1),
            mail_text_retention_days: config.mail_text_retention_days.max(1),
            health: health.clone(),
        };

        tokio::spawn(async move {
            // A minute is fine: these tasks are nightly, and the cost of a tick
            // is one indexed query against four rows.
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = sched_rx.recv() => break,
                    _ = tick.tick() => {
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

    tokio::spawn(async move {
        info!("Queue worker pool active ({max_concurrency} downloads, {max_encoders} encoders)");
        let mut worker_seq: u64 = 0;

        loop {
            // Capacity first. This await is where an idle daemon sits.
            let permit = tokio::select! {
                _ = worker_rx.recv() => {
                    info!("Worker pool shutting down.");
                    break;
                }
                p = async {
                    let job_slot = in_flight.clone().acquire_owned().await?;
                    let download_slot = semaphore.clone().acquire_owned().await?;
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

            let job = match repo_worker.lease_job(&owner, LEASE_SECS) {
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
                eng.hold_download_slot(job_id, download_slot);

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

                let outcome = run_job(&rep, &eng, &owner, job, cancel.clone()).await;
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

                // A job that ended before reaching the encoder still holds it.
                eng.release_download_slot(job_id);
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
) -> Result<()> {
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
    // yt-dlp already said it cannot read this page: skip the download
    // attempt that would only say so again (one more yt-dlp start and page
    // fetch, several seconds) and go straight to the sniffer.
    let mut known_unsupported = false;

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
    if is_web && page_referer.is_none() && !omni_broadcast::downloader::is_video_platform(&orig_url) {
        let scan = engine.page_videos(&orig_url).await;
        known_unsupported = scan.unsupported;
        let videos = scan.videos;
        if videos.len() > 1 {
            info!("Job #{job_id}: the page holds {} videos; this job takes the first", videos.len());
            repo.record_event(
                job_id,
                "INFO",
                Some(JobStage::Extract),
                &format!("The page holds {} videos; this job downloads the first: {}", videos.len(), videos[0]),
            )?;
            if !still_ours(repo, job_id, owner) {
                warn!("Job #{job_id}: no longer ours; not queuing the article's other videos");
                return Ok(());
            }
            omni_broadcast::article::queue_article_siblings(repo, owner, &mut job, &videos[0], &videos);
            job.url = videos[0].clone();
            page_referer = Some(orig_url.clone());
        }
    }
    if !still_ours(repo, job_id, owner) {
        warn!("Job #{job_id}: no longer ours; not downloading");
        return Ok(());
    }
    let mut process_result = if known_unsupported {
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
            .process_job_cancellable(job.clone(), owner, page_referer.as_deref(), None, None, Some(cancel.clone()))
            .await
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
    let sniff = match &process_result {
        Err(e) => classify_pipeline_error(e).should_sniff(&orig_url),
        Ok(()) => false,
    };
    if sniff && (orig_url.starts_with("http://") || orig_url.starts_with("https://")) {
        info!("Job #{job_id}: direct download failed; trying the stream sniffer.");
        if !repo.set_stage(job_id, owner, JobStage::Extract)? {
            warn!("Job #{job_id}: no longer ours; not sniffing");
            return Ok(());
        }
        repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Extract),
            "Direct download failed; sniffing the page for a stream",
        )?;

        if cancel.is_cancelled() {
            return Ok(());
        }
        match StreamSniffer::extract_media_bundle(&orig_url, 25).await {
            Ok(bundle) => {
                // An article's embedded posts are not all videos: the first
                // X post in a news247 article was a photo, became the job's
                // video, and sent the job to review while its two real
                // videos were delivered as siblings (P3.13). Ask yt-dlp
                // about each post first; keep the ones it cannot rule out.
                let streams = without_videoless_posts(repo, engine, job_id, &bundle.all_streams, &cancel).await;
                if cancel.is_cancelled() {
                    return Ok(());
                }
                let primary = if streams.contains(&bundle.primary_stream) {
                    Some(bundle.primary_stream.clone())
                } else {
                    streams.first().cloned()
                };
                match primary {
                    Some(primary) => {
                        info!("Job #{job_id}: sniffer found {primary}");
                        repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Sniffed stream: {primary}"))?;
                        let mut retry_job = job.clone();
                        if !still_ours(repo, job_id, owner) {
                            warn!("Job #{job_id}: no longer ours; not queuing the article's other videos");
                            return Ok(());
                        }
                        omni_broadcast::article::queue_article_siblings(repo, owner, &mut retry_job, &primary, &streams);
                        retry_job.url = primary;
                        if !repo.set_stage(job_id, owner, JobStage::Download)? {
                            warn!("Job #{job_id}: no longer ours; not downloading the sniffed stream");
                            return Ok(());
                        }
                        process_result = engine
                            .process_job_cancellable(
                                retry_job,
                                owner,
                                Some(&bundle.referer),
                                Some(&bundle.user_agent),
                                bundle.cookies.as_deref(),
                                Some(cancel.clone()),
                            )
                            .await;
                    }
                    None => {
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
                repo.record_event(
                    job_id,
                    "ERROR",
                    Some(JobStage::Extract),
                    &format!("Sniffer found no stream: {sniff_err}"),
                )?;
                if let Err(previous) = std::mem::replace(&mut process_result, Ok(())) {
                    process_result = Err(after_failed_sniff(previous, sniff_err));
                }
            }
        }
    }

    if lost_lease(&process_result) {
        warn!("Job #{job_id}: no longer ours; nothing more to do");
        return Ok(());
    }

    match process_result {
        Ok(()) => {
            let delivered = repo
                .get_job(job_id)?
                .and_then(|j| j.file_path)
                .filter(|p| !p.is_empty());
            repo.finish(
                job_id,
                owner,
                JobStatus::Completed,
                None,
                None,
                delivered.as_deref(),
            )?;
            Ok(())
        }
        Err(e) => {
            // The classified code decides whether waiting could possibly help
            // (plan P1.9). Retrying a deleted video burns worker slots and
            // delays telling the journalist their link is dead; retrying a
            // dropped connection usually just works.
            let code = classify_pipeline_error(&e);
            let attempts = repo.get_job(job_id)?.map(|j| j.attempts).unwrap_or(1);
            let max_attempts = repo.get_job(job_id)?.map(|j| j.max_attempts).unwrap_or(3);
            let message = readable_cause(&e);

            // The full context chain, which carries each stage's stderr tail,
            // into the job's own timeline (plan P6.1). `error_message` is the
            // one line the MCR card shows; this is what an engineer needs an
            // hour later, and putting it on the job means it survives log
            // rotation and does not require finding the right file.
            let diagnostic: String = format!("{e:#}").chars().take(8192).collect();
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
                repo.record_event(job_id, "ERROR", None, code.hint_el())?;
                repo.finish(
                    job_id,
                    owner,
                    status,
                    Some(code.as_str()),
                    Some(&message),
                    None,
                )?;
            }
            Ok(())
        }
    }
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
fn lost_lease(result: &anyhow::Result<()>) -> bool {
    matches!(result, Err(e) if e.downcast_ref::<omni_broadcast::pipeline::LeaseLost>().is_some())
}

/// The one readable line for a failure's `error_message` (P7.15): the root
/// cause, without the code strings the pipeline wraps around it.
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
struct MaintenanceContext {
    bin_dir: PathBuf,
    temp_path: PathBuf,
    ytdl_channel: String,
    ytdl_enabled: bool,
    adblock_enabled: bool,
    gate: UpdateGate,
    retention_days: i64,
    mail_text_retention_days: i64,
    health: HealthState,
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
            let ytdl = nightly_ytdl_update(&dep_mgr, &ctx.ytdl_channel, &ctx.gate, repo).await;
            let deno = nightly_deno_update(&dep_mgr, &ctx.gate, repo, &ctx.health).await;
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
            if failing > 0 {
                anyhow::bail!("{failing} self-check link(s) not working; see Administration → Self-check");
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
            match repo.purge_login_attempts(90) {
                Ok(n) if n > 0 => info!("Retention: removed {n} old login record(s)"),
                Ok(_) => {}
                Err(e) => warn!("Retention: login records not purged: {e:#}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::health::Health;
    use omni_core::models::QueueWatch;

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
        let lost = Err(anyhow::Error::new(omni_broadcast::pipeline::LeaseLost { job_id: 7, stage: JobStage::Rewrap })
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
}
