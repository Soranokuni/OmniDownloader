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
    /// Manage the Windows Service (install, uninstall, start, stop, status)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

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
    ));

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
    let web_state = AppState::new(repo.clone(), config.clone(), config_path.to_path_buf())
        .with_secret_store(secret_store.clone())
        .with_health(health.clone())
        .with_llm(live_llm.clone());
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
    let max_concurrency = config.max_concurrent_downloads.clamp(1, 10);
    let semaphore = Arc::new(Semaphore::new(max_concurrency));
    let mut worker_rx = shutdown_tx.subscribe();
    let repo_worker = repo.clone();
    let engine_worker = broadcast_engine.clone();
    let worker_hostname = hostname.clone();
    let worker_gate = update_gate.clone();
    let worker_encoder_ok = encoder_ok.clone();

    tokio::spawn(async move {
        info!("Queue worker pool active (concurrency: {max_concurrency})");
        let mut worker_seq: u64 = 0;

        loop {
            // Capacity first. This await is where an idle daemon sits.
            let permit = tokio::select! {
                _ = worker_rx.recv() => {
                    info!("Worker pool shutting down.");
                    break;
                }
                p = semaphore.clone().acquire_owned() => match p {
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

            tokio::spawn(async move {
                // Held for the whole job, including every error path: dropping
                // it is what tells a waiting updater the downloader is idle.
                let _gate_pass = gate_pass;
                let job_id = job.id;

                // Keep the lease alive while we work. If it stops succeeding we
                // have lost the job -- the reaper requeued it, or an operator
                // cancelled it -- and must stop rather than deliver a file for a
                // job somebody else now owns.
                let cancel = CancellationToken::new();
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

                let outcome = run_job(&rep, &eng, &owner, job).await;
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

                drop(permit);
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

/// Host component of a lease owner string.
///
/// Recovery only requeues jobs whose owner starts with this host, so two
/// daemons sharing a database never steal each other's work.
fn hostname_for_lease() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string())
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

    repo.set_stage(job_id, owner, JobStage::Download)?;
    let mut process_result = engine.process_job(job.clone(), owner).await;

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
        repo.set_stage(job_id, owner, JobStage::Extract)?;
        repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Extract),
            "Direct download failed; sniffing the page for a stream",
        )?;

        match StreamSniffer::extract_media_bundle(&orig_url, 25).await {
            Ok(bundle) => {
                info!("Job #{job_id}: sniffer found {}", bundle.primary_stream);
                repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Extract),
                    &format!("Sniffed stream: {}", bundle.primary_stream),
                )?;
                let mut retry_job = job.clone();
                omni_broadcast::article::queue_article_siblings(repo, owner, &mut retry_job, &bundle.primary_stream, &bundle.all_streams);
                retry_job.url = bundle.primary_stream;
                repo.set_stage(job_id, owner, JobStage::Download)?;
                process_result = engine
                    .process_job_with_context(
                        retry_job,
                        owner,
                        Some(&bundle.referer),
                        Some(&bundle.user_agent),
                        bundle.cookies.as_deref(),
                    )
                    .await;
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
            let message = format!("{e}");

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
    // Resume before reporting, so a logging failure cannot leave the queue
    // paused.
    gate.resume();

    match outcome {
        Ok(path) => {
            info!("Nightly yt-dlp updated to {:?}", path);
            let _ = repo.log_audit(
                "INFO",
                "SYSTEM",
                &format!(
                    "yt-dlp updated ({} channel, sha256 {}); previous build kept for rollback",
                    channel,
                    &staged.sha256[..16]
                ),
            );
            TaskOutcome::Ok
        }
        Err(e) => {
            warn!("Nightly yt-dlp update failed to install: {e:#}");
            let _ = repo.log_audit("ERROR", "SYSTEM", &format!("yt-dlp update failed: {e}"));
            TaskOutcome::Failed
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
            Ok(nightly_ytdl_update(&dep_mgr, &ctx.ytdl_channel, &ctx.gate, repo).await)
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
}
