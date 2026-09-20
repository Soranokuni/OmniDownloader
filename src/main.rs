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
    if process_result.is_err() && (orig_url.starts_with("http://") || orig_url.starts_with("https://")) {
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
