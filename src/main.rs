use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Semaphore};
use tracing::{error, info, warn};

use omni_broadcast::pipeline::BroadcastEngine;
use omni_browser::agent::{ComputerUseAgentPlaceholder, FileLockerResolver};
use omni_browser::sniffer::StreamSniffer;
use omni_core::config::AppConfig;
use omni_core::paths::AppPaths;
use omni_core::dependencies::DependencyManager;
use omni_core::models::JobStatus;
use omni_core::repository::Repository;
use omni_email::watcher::EmailWatcher;
use omni_web::server::WebServer;
use omni_web::state::AppState;

#[derive(Parser)]
#[command(name = "omni-ingest")]
#[command(author = "OmniDownloader Team")]
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
    Install,
    Uninstall,
    Start,
    Stop,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "info,omni_web=info,omni_email=info,omni_broadcast=info,omni_browser=info",
            )
        })
        .add_directive("chromiumoxide=off".parse().unwrap())
        .add_directive("chromiumoxide::conn=off".parse().unwrap())
        .add_directive("chromiumoxide::handler=off".parse().unwrap());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

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
                ServiceAction::Install => omni_cli::ServiceSubcommand::Install,
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
    info!("   Sony XDCAM HD422 PAL 1080i50 + Dalet OP1a Integration    ");
    info!("============================================================");

    let config_path = paths.config.clone();
    let config = AppConfig::load_from_file(&config_path)
        .with_context(|| format!("Failed loading configuration from {:?}", config_path))?;

    // Every one of these is absolute: relative entries resolve against the
    // install root, absolute and UNC entries (the Dalet share) pass through.
    let db_path = paths.resolve(&config.database_path);
    let temp_path = paths.resolve(&config.temp_path);
    let watchfolder_path = paths.resolve(&config.watchfolder_path);
    let bin_dir = paths.resolve(&config.bin_dir);

    tokio::fs::create_dir_all(&temp_path).await?;
    tokio::fs::create_dir_all(&watchfolder_path).await?;
    tokio::fs::create_dir_all(&bin_dir).await?;

    info!("Initializing SQLite database at: {:?}", db_path);
    let repo = Repository::new(&db_path)?;

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

    // 1. Start Embedded Web Server
    let web_state = AppState::new(repo.clone(), config.clone(), config_path.to_path_buf());
    let web_host = config.web_host.clone();
    let web_port = config.web_port;
    let web_rx = shutdown_tx.subscribe();

    tokio::spawn(async move {
        if let Err(e) = WebServer::run(web_state, &web_host, web_port, web_rx).await {
            error!("Web server terminated with error: {:?}", e);
        }
    });

    // 2. Start Email Monitoring Watchdog
    if !config.email_address.is_empty() {
        let email_watcher = Arc::new(EmailWatcher::new(config.clone(), repo.clone()));
        let email_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            email_watcher.start_polling_loop(email_rx).await;
        });
    } else {
        info!("Email monitoring disabled (no email address configured in config.json).");
    }

    // 3. Start Nightly yt-dlp & Adblock Auto-updater
    if config.ytdl_auto_update_nightly || config.adblock_auto_update_nightly {
        let mut updater_rx = shutdown_tx.subscribe();
        let dep_mgr_clone = DependencyManager::new(&bin_dir);
        let channel = config.ytdl_channel.clone();
        let ytdl_enabled = config.ytdl_auto_update_nightly;
        let adblock_enabled = config.adblock_auto_update_nightly;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tokio::select! {
                    _ = updater_rx.recv() => break,
                    _ = interval.tick() => {
                        let now = chrono::Local::now();
                        if now.format("%H").to_string() == "03" {
                            if ytdl_enabled {
                                info!("Running scheduled nightly yt-dlp auto-update check...");
                                match dep_mgr_clone.download_ytdl(&channel).await {
                                    Ok(dest) => info!("Nightly yt-dlp updated successfully to {:?}", dest),
                                    Err(e) => warn!("Nightly yt-dlp update check failed: {}", e),
                                }
                            }
                            if adblock_enabled {
                                info!("Running scheduled nightly HaGeZi + Greek AdBlock update check...");
                                let blocker = omni_browser::UnifiedAdBlocker::global();
                                match blocker.update_blocklists().await {
                                    Ok(stats) => info!("Nightly AdBlock updated ({} active domains)", stats.total_domains),
                                    Err(e) => warn!("Nightly AdBlock update check failed: {}", e),
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // 4. Start Worker Pool Queue Leaser
    let max_concurrency = config.max_concurrent_downloads.clamp(1, 10);
    let semaphore = Arc::new(Semaphore::new(max_concurrency));
    let mut worker_rx = shutdown_tx.subscribe();
    let repo_worker = repo.clone();
    let engine_worker = broadcast_engine.clone();

    tokio::spawn(async move {
        info!("Queue Worker pool active (concurrency: {})", max_concurrency);
        loop {
            tokio::select! {
                _ = worker_rx.recv() => {
                    info!("Worker pool shutting down.");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(1500)) => {
                    match repo_worker.lease_next_pending_job() {
                        Ok(Some(job)) => {
                            let sem = semaphore.clone();
                            let eng = engine_worker.clone();
                            let rep = repo_worker.clone();

                            tokio::spawn(async move {
                                let permit = match sem.acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => return,
                                };

                                let job_id = job.id;
                                let orig_url = job.url.clone();

                                // Check for file locker URLs requiring autonomous Computer-Use agent
                                let locker_resolver = ComputerUseAgentPlaceholder::new(None, None);
                                if locker_resolver.can_handle(&orig_url) {
                                    warn!("Job #{} matches file locker domain. Routing to MCR resolution desk.", job_id);
                                    let _ = rep.update_job_status(
                                        job_id,
                                        JobStatus::RequiresReview,
                                        Some("File-locker detected (WeTransfer/AirBridge). Manual download or Computer-Use resolution required."),
                                        None,
                                        None,
                                    );
                                    let _ = rep.log_audit(
                                        "WARN",
                                        "COMPUTER_USE",
                                        &format!("Job #{} file locker: {}", job_id, orig_url),
                                    );
                                    drop(permit);
                                    return;
                                }

                                // Process job with broadcast pipeline.
                                // If direct download fails and URL appears to be an interactive web portal,
                                // fallback to StreamSniffer to extract .m3u8 stream.
                                let mut process_result = eng.process_job(job.clone()).await;

                                if process_result.is_err() {
                                    let is_web_url = orig_url.starts_with("http://")
                                        || orig_url.starts_with("https://");

                                    if is_web_url {
                                        info!("Direct download failed for web URL ({}). Attempting Headless Browser StreamSniffer fallback...", orig_url);
                                        let _ = rep.update_job_progress(job_id, 0.0, "Sniffing stream...", "--:--");

                                        match StreamSniffer::extract_media_bundle(&orig_url, 25).await {
                                            Ok(bundle) => {
                                                info!("StreamSniffer recovered primary stream URL: {}", bundle.primary_stream);
                                                let mut retry_job = job.clone();
                                                retry_job.url = bundle.primary_stream;
                                                let _ = rep.update_job_status(job_id, JobStatus::Downloading, None, None, None);
                                                process_result = eng.process_job_with_context(
                                                    retry_job,
                                                    Some(&bundle.referer),
                                                    Some(&bundle.user_agent),
                                                    bundle.cookies.as_deref(),
                                                ).await;
                                            }
                                            Err(sniff_err) => {
                                                warn!("StreamSniffer fallback also failed: {}", sniff_err);
                                            }
                                        }
                                    }
                                }

                                if let Err(e) = process_result {
                                    error!("Broadcast pipeline failed for Job #{}: {:?}", job_id, e);
                                }

                                drop(permit);
                            });
                        }
                        Ok(None) => {}
                        Err(e) => {
                            error!("Error leasing next pending job: {:?}", e);
                        }
                    }
                }
            }
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
