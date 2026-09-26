use anyhow::{Context, Result};
use inquire::{Confirm, CustomType, Password, Text};
use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_core::secrets::{keys as secret_keys, SecretStore};
use omni_service::{
    install_service, needs_network_identity, query_service_status, start_service, stop_service,
    uninstall_service, ServiceAccount,
};
use std::env;
use std::path::Path;


pub enum ServiceSubcommand {
    Install {
        /// Account to run as, e.g. `DOMAIN\svc_omni`. `None` = LocalSystem.
        account: Option<String>,
        /// Watchfolder path, so the installer can warn when LocalSystem will
        /// not be able to reach it.
        watchfolder: Option<String>,
    },
    Uninstall,
    Start,
    Stop,
    Status,
}

pub fn handle_service_command(cmd: ServiceSubcommand) -> Result<()> {
    match cmd {
        ServiceSubcommand::Install {
            account,
            watchfolder,
        } => {
            let current_exe = env::current_exe().context("Failed to get current executable path")?;
            println!("Installing Windows Service with binary: {}", current_exe.display());

            let account = resolve_service_account(account.as_deref(), watchfolder.as_deref())?;
            install_service(&current_exe, account.as_ref())?;

            println!("✓ Windows Service 'OmniIngestService' installed successfully!");
            print_privilege_checklist(account.as_ref(), watchfolder.as_deref());
            println!("\nTo start the service, run: omni-ingest service start");
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

/// Decide which account the service runs as, prompting for a password when the
/// account needs one (plan P2.8, defect W-13).
fn resolve_service_account(
    account: Option<&str>,
    watchfolder: Option<&str>,
) -> Result<Option<ServiceAccount>> {
    let Some(name) = account.map(str::trim).filter(|s| !s.is_empty()) else {
        // LocalSystem. Fine for a local watchfolder; broken for a UNC one,
        // because LocalSystem authenticates to SMB as the *computer account*
        // and a file server will usually refuse it. That failure happens at
        // the last step of the pipeline, after a correct MXF already exists,
        // so it looks like a delivery bug rather than a permissions one.
        if watchfolder.map(needs_network_identity).unwrap_or(false) {
            println!();
            println!("! The watchfolder is a network share, and the service will run as");
            println!("  LocalSystem, which authenticates to SMB as this computer's account.");
            println!("  Most file servers refuse that, and the failure only appears at");
            println!("  delivery — after a correct MXF has already been produced.");
            println!();
            println!("  Re-run with a domain account that has Modify on the share:");
            println!("    omni-ingest service install --account \"DOMAIN\\svc_omni\"");
            println!();
            let proceed = Confirm::new("Install as LocalSystem anyway?")
                .with_default(false)
                .prompt()?;
            if !proceed {
                anyhow::bail!("Installation cancelled.");
            }
        }
        return Ok(None);
    };

    if ServiceAccount::is_passwordless_builtin(name) {
        // Prompting here and passing an empty string makes CreateService fail
        // with a misleading "logon failure".
        println!("  {name} is a built-in account; no password is needed.");
        return Ok(Some(ServiceAccount {
            name: name.to_string(),
            password: None,
        }));
    }

    let password = Password::new(&format!("Password for {name}:"))
        .with_display_mode(inquire::PasswordDisplayMode::Masked)
        .without_confirmation()
        .with_help_message("Passed to the Service Control Manager; never written to disk by us")
        .prompt()?;

    Ok(Some(ServiceAccount::with_password(name, password)))
}

/// What the operator has to grant before the service will actually work.
///
/// Printed at install time rather than left in a document, because the failures
/// it prevents all look like something else: a watchfolder the account cannot
/// write to looks like a delivery bug, and a missing "Log on as a service"
/// right looks like the service silently refusing to start.
fn print_privilege_checklist(account: Option<&ServiceAccount>, watchfolder: Option<&str>) {
    let Some(account) = account else {
        return;
    };
    println!();
    println!("Grant {} the following, or the service will fail:", account.name);
    println!("  • 'Log on as a service'  (secpol.msc → Local Policies → User Rights Assignment)");
    println!("  • Modify on the install directory's data\\, temp\\, logs\\ and archive\\");
    if let Some(w) = watchfolder {
        println!("  • Modify on the watchfolder: {w}");
        if needs_network_identity(w) {
            println!("    (a share permission AND an NTFS permission — both are checked)");
        }
    } else {
        println!("  • Modify on the watchfolder");
    }
    println!("  • Write on temp\\browser-profile (the headless browser runs as this account)");
    println!();
    println!("Note: secrets are encrypted with DPAPI at machine scope, so changing the");
    println!("service account does NOT invalidate data\\secrets.bin.");
}

/// `omni-ingest secrets …` (plan P2.6).
pub enum SecretSubcommand {
    /// Set a secret, prompting for the value so it never appears in a command
    /// line, a shell history, or the process list.
    Set { key: String },
    /// Remove a secret.
    Clear { key: String },
    /// List which secrets are set. Values are never printed.
    List,
}

pub fn handle_secrets_command(store: &SecretStore, cmd: SecretSubcommand) -> Result<()> {
    match cmd {
        SecretSubcommand::List => {
            println!("\nSecret store: {}", store.path().display());
            for (key, is_set) in store.status() {
                println!("  {:<24} {}", key, if is_set { "set" } else { "not set" });
            }
            println!("\nValues are never displayed. To replace one: omni-ingest secrets set <key>");
        }
        SecretSubcommand::Set { key } => {
            ensure_known_key(&key)?;
            // Piped stdin (`... | omni-ingest secrets set graph.client_secret`) for a
            // scripted install; an interactive masked prompt otherwise. Either
            // way the value never appears as a command-line argument, where it
            // would be visible in the process list and the shell history.
            let value = match read_piped_secret()? {
                Some(piped) => piped,
                None => Password::new(&format!("Value for {key}:"))
                    .with_display_mode(inquire::PasswordDisplayMode::Masked)
                    .with_help_message("Stored encrypted in data/secrets.bin; not echoed")
                    .prompt()?,
            };
            if value.is_empty() {
                println!("! Empty value; nothing was changed. Use `secrets clear` to remove one.");
                return Ok(());
            }
            store.set(&key, &value)?;
            println!("✓ {key} stored.");
        }
        SecretSubcommand::Clear { key } => {
            ensure_known_key(&key)?;
            store.remove(&key)?;
            println!("✓ {key} cleared.");
        }
    }
    Ok(())
}

/// Read a secret from a pipe, or `None` when stdin is a terminal.
///
/// A trailing newline is stripped, because `echo x | ...` adds one and a
/// password with an invisible newline on the end is a support call that takes
/// an afternoon.
fn read_piped_secret() -> Result<Option<String>> {
    use std::io::{IsTerminal, Read};

    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    stdin.read_to_string(&mut buf)?;
    Ok(Some(buf.trim_end_matches(['\r', '\n']).to_string()))
}

fn ensure_known_key(key: &str) -> Result<()> {
    if secret_keys::ALL.contains(&key) {
        return Ok(());
    }
    anyhow::bail!(
        "Unknown secret `{}`. Known keys: {}",
        key,
        secret_keys::ALL.join(", ")
    )
}

pub fn run_setup_wizard(config_path_opt: Option<&str>) -> Result<()> {
    println!();
    println!("============================================================");
    println!("   OMNIDOWNLOADER BROADCAST INGEST ENGINE - SETUP WIZARD    ");
    println!("   Sony XDCAM HD422 PAL 1080i50 + RDD9 OP1a broadcast ingest   ");
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
    config.watchfolder_path = Text::new("Broadcast ingest watchfolder (play-out destination):")
        .with_default(&config.watchfolder_path)
        .with_help_message("Directory where atomic broadcast MXF assets will be dropped")
        .prompt()?;

    // 3. Temp dir
    config.temp_path = Text::new("Temporary Working Directory:")
        .with_default(&config.temp_path)
        .with_help_message("Directory for transient downloads and intermediate transcodes")
        .prompt()?;

    // 4. Email Ingest: the Office 365 mailbox through Microsoft Graph (P4.7).
    let enable_email = Confirm::new("Enable email ingest from the Office 365 mailbox (Microsoft Graph)?")
        .with_default(!config.graph.mailbox.trim().is_empty())
        .prompt()?;

    if enable_email {
        config.graph.tenant_id = Text::new("Directory (tenant) ID:")
            .with_default(&config.graph.tenant_id)
            .with_help_message("From the app registration's Overview page in Entra ID")
            .prompt()?;

        config.graph.client_id = Text::new("Application (client) ID:")
            .with_default(&config.graph.client_id)
            .prompt()?;

        config.graph.mailbox = Text::new("Ingest mailbox address:")
            .with_default(&config.graph.mailbox)
            .prompt()?;

        let secret = Password::new("Client secret value:")
            .without_confirmation()
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .with_help_message("Leave empty to keep the stored secret")
            .prompt()?;

        if !secret.is_empty() {
            // Into the encrypted store, never config.json (plan P2.6).
            // `config.graph.client_secret` is the runtime copy; `save_to_file`
            // cannot serialise it.
            let store = SecretStore::new(config.resolve_path("data/secrets.bin"));
            store
                .set(secret_keys::GRAPH_CLIENT_SECRET, &secret)
                .context("Failed storing the Graph client secret")?;
            config.graph.client_secret = secret;
            println!("  Client secret stored encrypted in data/secrets.bin.");
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
        // create_user hashes internally (repository.rs). Hashing here as well
        // stored a hash *of the hash*, so the account the wizard created could
        // never log in -- and the operator only found out at the login screen,
        // after the wizard had reported success (defect W-05).
        repo.create_user(
            &admin_email,
            &admin_password,
            UserRole::Admin,
            "System Administrator",
            None,
        )?;


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

            // Ask for the account here rather than after the fact: a UNC
            // watchfolder under LocalSystem produces a correct MXF and then
            // fails to deliver it, which reads as a pipeline bug.
            let account_name = Text::new("Service account (blank = LocalSystem):")
                .with_default("")
                .with_help_message(r"e.g. DOMAIN\svc_omni — required when the watchfolder is a network share")
                .prompt()?;
            let account = resolve_service_account(
                Some(account_name.as_str()),
                Some(config.watchfolder_path.as_str()),
            )?;

            if let Err(e) = install_service(&current_exe, account.as_ref()) {
                println!("! Service installation returned notice: {}", e);
                println!("  (You may need to run setup in an Administrator terminal to install services)");
            } else {
                println!("✓ Windows Service 'OmniIngestService' installed successfully!");
                print_privilege_checklist(account.as_ref(), Some(&config.watchfolder_path));
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
