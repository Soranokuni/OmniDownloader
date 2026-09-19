use anyhow::{Context, Result};
use inquire::{Confirm, CustomType, Password, Text};
use omni_core::auth::hash_password;
use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_service::{install_service, query_service_status, start_service, stop_service, uninstall_service};
use std::env;
use std::path::Path;


pub enum ServiceSubcommand {
    Install,
    Uninstall,
    Start,
    Stop,
    Status,
}

pub fn handle_service_command(cmd: ServiceSubcommand) -> Result<()> {
    match cmd {
        ServiceSubcommand::Install => {
            let current_exe = env::current_exe().context("Failed to get current executable path")?;
            println!("Installing Windows Service with binary: {}", current_exe.display());
            install_service(&current_exe)?;
            println!("✓ Windows Service 'OmniIngestService' installed successfully!");
            println!("To start the service, run: omni-ingest service start");
        }
        ServiceSubcommand::Uninstall => {
            println!("Uninstalling Windows Service...");
            uninstall_service()?;
            println!("✓ Windows Service 'OmniIngestService' uninstalled successfully.");
        }
        ServiceSubcommand::Start => {
            println!("Starting Windows Service...");
            start_service()?;
            println!("✓ Windows Service started successfully.");
        }
        ServiceSubcommand::Stop => {
            println!("Stopping Windows Service...");
            stop_service()?;
            println!("✓ Windows Service stopped successfully.");
        }
        ServiceSubcommand::Status => {
            let status = query_service_status()?;
            println!("OmniIngestService status: {}", status);
        }
    }
    Ok(())
}

pub fn run_setup_wizard(config_path_opt: Option<&str>) -> Result<()> {
    println!();
    println!("============================================================");
    println!("   OMNIDOWNLOADER BROADCAST INGEST ENGINE - SETUP WIZARD    ");
    println!("   Sony XDCAM HD422 PAL 1080i50 + Dalet OP1a Integration    ");
    println!("============================================================");
    println!();

    let target_config_path = config_path_opt.unwrap_or("config.json");
    let mut config = if Path::new(target_config_path).exists() {
        println!("Found existing config at '{}'. Loading defaults...", target_config_path);
        AppConfig::load_from_file(target_config_path).unwrap_or_default()
    } else {
        AppConfig::default()
    };

    // 1. Web Port
    config.web_port = CustomType::<u16>::new("Web Dashboard HTTP Port:")
        .with_default(config.web_port)
        .with_help_message("Port for MCR status watcher and Admin Panel (default 8080)")
        .prompt()?;

    // 2. Watchfolder
    config.watchfolder_path = Text::new("Dalet / Play-out Watchfolder Destination:")
        .with_default(&config.watchfolder_path)
        .with_help_message("Directory where atomic broadcast MXF assets will be dropped")
        .prompt()?;

    // 3. Temp dir
    config.temp_path = Text::new("Temporary Working Directory:")
        .with_default(&config.temp_path)
        .with_help_message("Directory for transient downloads and intermediate transcodes")
        .prompt()?;

    // 4. Email Ingest
    let enable_email = Confirm::new("Enable Outlook / IMAP email monitoring watchdog?")
        .with_default(!config.email_address.is_empty())
        .prompt()?;

    if enable_email {
        config.imap_server = Text::new("IMAP Host:")
            .with_default(&config.imap_server)
            .prompt()?;

        config.imap_port = CustomType::<u16>::new("IMAP Port:")
            .with_default(config.imap_port)
            .prompt()?;

        config.email_address = Text::new("Email Address / Username:")
            .with_default(&config.email_address)
            .prompt()?;

        let pw = Password::new("Email Password / App Password:")
            .without_confirmation()
            .with_help_message("Leave empty to keep existing password")
            .prompt()?;

        if !pw.is_empty() {
            config.email_password = pw;
        }

        config.email_poll_interval_secs = CustomType::<u64>::new("Email Polling Interval (seconds):")
            .with_default(config.email_poll_interval_secs)
            .prompt()?;
    }

    // 5. LLM Endpoint
    let config_llm = Confirm::new("Configure Local LLM Ingest Parser (Ollama / vLLM / OpenAI API)?")
        .with_default(true)
        .prompt()?;

    if config_llm {
        config.ollama_endpoint = Text::new("LLM API Endpoint:")
            .with_default(&config.ollama_endpoint)
            .with_help_message("e.g. http://localhost:11434/v1 or http://127.0.0.1:8000/v1")
            .prompt()?;

        config.ollama_model = Text::new("LLM Model Name:")
            .with_default(&config.ollama_model)
            .with_help_message("e.g. llama3:latest, mistral, gpt-4o-mini")
            .prompt()?;
    }

    // 6. Nightly yt-dlp auto-update
    config.ytdl_auto_update_nightly = Confirm::new("Enable nightly automatic yt-dlp updates (at 03:00)?")
        .with_default(config.ytdl_auto_update_nightly)
        .prompt()?;

    // 7. Save config
    config.save_to_file(target_config_path)?;
    println!("\n✓ Configuration saved to '{}'.", target_config_path);

    // 8. Seed Admin user in SQLite
    let db_path = config.resolve_path(&config.database_path);
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let repo = Repository::new(&db_path)?;

    let users = repo.list_users().unwrap_or_default();
    let has_admin = users.iter().any(|u| u.role == UserRole::Admin);

    if !has_admin {
        println!("\nNo administrator account found in database. Let's create one now.");
        let admin_email = Text::new("Admin Email:")
            .with_default("admin@station.gr")
            .prompt()?;

        let admin_password = Password::new("Admin Password:")
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .prompt()?;
        let pass_hash = hash_password(&admin_password)?;
        repo.create_user(&admin_email, &pass_hash, UserRole::Admin, "System Administrator", None)?;



        println!("✓ Administrator user '{}' created.", admin_email);
    }

    // 9. Windows Service Installation Prompt
    #[cfg(windows)]
    {
        let install_srv = Confirm::new("Install OmniDownloader as an automatic Windows Service now?")
            .with_default(false)
            .prompt()?;

        if install_srv {
            let current_exe = env::current_exe().context("Failed to get executable path")?;
            if let Err(e) = install_service(&current_exe) {
                println!("! Service installation returned notice: {}", e);
                println!("  (You may need to run setup in an Administrator terminal to install services)");
            } else {
                println!("✓ Windows Service 'OmniIngestService' installed successfully!");
                let start_now = Confirm::new("Start the Windows Service now?")
                    .with_default(true)
                    .prompt()?;
                if start_now {
                    if let Err(e) = start_service() {
                        println!("! Service start returned notice: {}", e);
                    } else {
                        println!("✓ Windows Service started and running in background!");
                    }
                }
            }
        }
    }

    println!("\nSetup complete! You can start OmniDownloader in daemon mode with:");
    println!("  omni-ingest run\n");
    Ok(())
}
