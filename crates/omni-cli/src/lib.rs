pub mod install;

use anyhow::{Context, Result};
use inquire::Password;
use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::paths::AppPaths;
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
        /// Account to run as, e.g. `DOMAIN\svc_omni`. `None` = the virtual
        /// account `NT SERVICE\OmniIngestService`; `LocalSystem` opts out.
        account: Option<String>,
        /// The installation, and its configuration when it can be read (the
        /// watchfolder warning and the folder permissions follow it).
        paths: AppPaths,
        config: Option<AppConfig>,
    },
    Uninstall,
    Start,
    Stop,
    Status,
}

pub async fn handle_service_command(cmd: ServiceSubcommand) -> Result<()> {
    match cmd {
        ServiceSubcommand::Install {
            account,
            paths,
            config,
        } => {
            let current_exe = env::current_exe().context("Failed to get current executable path")?;
            println!("Installing Windows Service with binary: {}", current_exe.display());

            let config = config.unwrap_or_default();
            let watchfolder = config.watchfolder_path.clone();
            let account = resolve_service_account(account.as_deref(), Some(watchfolder.as_str()))?;
            let previous = omni_service::installed_service_account();
            install_service(&current_exe, account.as_ref())?;
            // The same rights as the installer: without them the new account
            // cannot open data\ and temp\ if they were locked for LocalSystem.
            install::apply_acls(&paths, &config, account.as_ref(), previous.as_deref(), &current_exe).await;

            println!("✓ Windows Service 'OmniIngestService' installed successfully!");
            print_privilege_checklist(account.as_ref(), Some(watchfolder.as_str()));
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
    // One decision for both installers (P2.16): the virtual account unless
    // `--account` says otherwise, the network-share warning, and a password
    // prompt only for an account that takes one (an empty password for a
    // built-in makes CreateService fail with a misleading "logon failure").
    install::choose_account(account, watchfolder.unwrap_or(""), false)
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
    let named_user = !ServiceAccount::is_passwordless_builtin(&account.name);
    println!();
    println!("Grant {} the following, or the service will fail:", account.name);
    if named_user {
        println!("  • 'Log on as a service'  (secpol.msc → Local Policies → User Rights Assignment)");
    }
    println!("  • Modify on the install directory's data\\, temp\\, logs\\, archive\\ and bin\\");
    println!("  • Modify on the file config.json (the admin pages save it); not on the install directory itself");
    if let Some(w) = watchfolder {
        println!("  • Modify on the watchfolder: {w}");
        if needs_network_identity(w) {
            println!("    (a share permission AND an NTFS permission — both are checked)");
        }
    } else {
        println!("  • Modify on the watchfolder");
    }
    println!("  • %TEMP%\\omni-sniffer (the headless browser's profile) is in this account's own profile temp;");
    println!("    nothing to grant for a virtual account");
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

/// Account recovery from the machine itself (`omni-ingest admin …`).
///
/// Whoever can run this can already read the database file, so it adds no
/// new access; it only saves them from editing SQL by hand when the one
/// administrator's password (or address) is forgotten.
pub enum AdminSubcommand {
    /// List every account: address, role, active. No password data.
    ListUsers,
    /// Set a new password for one account and end all its sessions.
    ResetPassword { email: String },
    /// Deactivate (sign out, refuse sign-in) or reactivate an account. The
    /// last active administrator is never deactivated.
    SetActive { email: String, active: bool },
    /// Create an administrator (P13.3): the way back in when no administrator
    /// can sign in. Refused while an active one exists, unless `force`.
    CreateAdmin { username: String, force: bool },
}

/// Create the administrator `username` with `password` (P13.3: no install is
/// ever locked out). Refused while an active administrator exists unless
/// `force`, for a login in use or the first-run `admin`, and for a password the
/// panel would refuse. Writes an audit row naming the login, never the
/// password. Returns the line to print.
pub fn create_admin(repo: &Repository, username: &str, password: &str, force: bool) -> Result<String> {
    let login = username.trim().to_lowercase();
    if login.chars().count() < 3 || login.chars().count() > 64 || login.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!("The login must be 3 to 64 characters, without spaces.");
    }
    if login == omni_core::repository::DEFAULT_ADMIN_LOGIN {
        anyhow::bail!("Choose a login other than \"admin\": that name is the first-run account.");
    }
    if repo.has_active_admin()? && !force {
        anyhow::bail!(
            "An active administrator already exists; sign in with it, or use `omni-ingest admin reset-password <login>`. \
             Add --force to create another administrator anyway."
        );
    }
    if repo.list_users()?.iter().any(|u| u.email.trim().eq_ignore_ascii_case(&login)) {
        anyhow::bail!("The login `{login}` is already in use; see `omni-ingest admin list-users`.");
    }
    omni_core::auth::validate_password(password).map_err(anyhow::Error::msg)?;
    repo.create_user(&login, password, UserRole::Admin, "Administrator", None)?;
    let _ = repo.log_audit("WARN", "ADMIN", &format!("Administrator {login} created from the command line"));
    Ok(format!("✓ Administrator {login} created. Sign in at /admin with it."))
}

pub fn handle_admin_command(repo: &Repository, cmd: AdminSubcommand) -> Result<()> {
    match cmd {
        AdminSubcommand::ListUsers => {
            let users = repo.list_users()?;
            if users.is_empty() {
                println!(
                    "\nNo accounts. Start the service: a new installation gets the first-run account Admin / Admin, \
                     which signs in at http://127.0.0.1:<port>/admin from this computer and opens the setup wizard. \
                     If it does not appear, create an administrator with `omni-ingest admin create-admin <login>`."
                );
                return Ok(());
            }
            if !users.iter().any(|u| u.is_active && u.role == UserRole::Admin) {
                println!(
                    "\nNo active administrator. Reactivate one (`omni-ingest admin activate <login>`) \
                     or create one (`omni-ingest admin create-admin <login>`)."
                );
            }
            println!("\n{:<40} {:<10} {}", "Sign-in address", "Role", "Active");
            for u in users {
                println!("{:<40} {:<10} {}", u.email, u.role.as_str(), if u.is_active { "yes" } else { "NO" });
            }
        }
        AdminSubcommand::SetActive { email, active } => {
            let users = repo.list_users()?;
            let wanted = email.trim().to_lowercase();
            let Some(user) = users.iter().find(|u| u.email.trim().to_lowercase() == wanted) else {
                anyhow::bail!("No account signs in as `{}`. See `omni-ingest admin list-users`.", email.trim());
            };
            if !active
                && user.role == UserRole::Admin
                && !users.iter().any(|u| u.id != user.id && u.is_active && u.role == UserRole::Admin)
            {
                anyhow::bail!("{} is the last active administrator; create or reactivate another first.", user.email);
            }
            repo.set_user_active_status(user.id, active)?;
            if !active {
                repo.delete_sessions_for_user(user.id)?;
            }
            let _ = repo.log_audit(
                "WARN",
                "ADMIN",
                &format!("Account {} {} from the command line", user.email, if active { "reactivated" } else { "deactivated" }),
            );
            println!("✓ {} {}.", user.email, if active { "reactivated" } else { "deactivated and signed out" });
        }
        AdminSubcommand::CreateAdmin { username, force } => {
            // Checked before the prompt, so nobody types a password for nothing.
            if repo.has_active_admin()? && !force {
                return create_admin(repo, &username, "", false).map(|_| ());
            }
            // Piped for scripts, masked and typed twice otherwise; never an argument.
            let password = match read_piped_secret()? {
                Some(piped) => piped,
                None => Password::new(&format!("Password for {}:", username.trim()))
                    .with_display_mode(inquire::PasswordDisplayMode::Masked)
                    .with_custom_confirmation_message("Type it again:")
                    .with_help_message("At least 12 characters")
                    .prompt()?,
            };
            println!("{}", create_admin(repo, &username, &password, force)?);
        }
        AdminSubcommand::ResetPassword { email } => {
            let wanted = email.trim().to_lowercase();
            let Some(user) = repo.list_users()?.into_iter().find(|u| u.email.trim().to_lowercase() == wanted) else {
                let known: Vec<String> = repo.list_users()?.into_iter().map(|u| u.email).collect();
                anyhow::bail!(
                    "No account signs in as `{}`. Accounts: {}",
                    email.trim(),
                    if known.is_empty() { "none".to_string() } else { known.join(", ") }
                );
            };
            // Piped for scripts, masked and confirmed otherwise; never an
            // argument, which would sit in the shell history.
            let password = match read_piped_secret()? {
                Some(piped) => piped,
                None => Password::new(&format!("New password for {}:", user.email))
                    .with_display_mode(inquire::PasswordDisplayMode::Masked)
                    .with_custom_confirmation_message("Type it again:")
                    .with_help_message("At least 12 characters")
                    .prompt()?,
            };
            omni_core::auth::validate_password(&password).map_err(anyhow::Error::msg)?;
            repo.update_user_password(user.id, &password)?;
            let _ = repo.log_audit(
                "WARN",
                "ADMIN",
                &format!("Password for {} reset from the command line", user.email),
            );
            println!("✓ Password for {} changed; every session of that account was ended.", user.email);
            if !user.is_active {
                println!("! This account is deactivated and still cannot sign in; reactivate it from another admin account.");
            }
            println!(
                "  If sign-in was refused as too many attempts, wait up to an hour or restart the daemon: \
                 the attempt counter is in memory."
            );
        }
    }
    Ok(())
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

/// `omni-ingest setup --answer <file>` (P13.3): apply an answer file (the admin
/// page's «Download answer file», or one written for Setup's `/answer`) to this
/// installation's `config.json`, through the same rules as Setup
/// (`omni_core::answer_file`). The interactive setup is the wizard on /admin.
///
/// The settings the file does not name keep their values. `setup_completed`
/// never changes and no secret is taken (a file naming one is refused). The
/// previous file is kept as `config.json.prev`. Returns the lines to print.
///
/// The same rules as the installer and the panel (P13.3b): a watchfolder the
/// service cannot use is refused with the installer's text; an online LLM
/// endpoint must be `https://`, and when the endpoint names another provider
/// the stored LLM key is removed, so it is never sent to the new host. While
/// the service runs it is refused unless `force` (the daemon would write its
/// own settings over the file on its next save).
pub fn apply_answer_file(paths: &AppPaths, answer: &Path, force: bool) -> Result<Vec<String>> {
    let running = omni_service::query_service_status().map(|s| s != "STOPPED").unwrap_or(false);
    let store = SecretStore::new(paths.resolve("data/secrets.bin"));
    apply_answer_file_with(paths, answer, &omni_core::paths::SystemDrives, &store, running, force)
}

/// [`apply_answer_file`] with the drive lookups, the secret store and the
/// service state given (tests).
pub fn apply_answer_file_with(
    paths: &AppPaths,
    answer: &Path,
    drives: &dyn omni_core::paths::DriveLookup,
    store: &SecretStore,
    service_running: bool,
    force: bool,
) -> Result<Vec<String>> {
    if service_running && !force {
        anyhow::bail!(
            "The OmniIngest service is running: on its next settings save it would write its own settings over these. \
             Stop it first (omni-ingest service stop), apply the file, then start it; or add --force."
        );
    }
    let text = std::fs::read_to_string(answer)
        .with_context(|| format!("Cannot read the answer file {}", answer.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("The answer file {} is not JSON", answer.display()))?;
    let existing = paths.config.exists();
    let base = if existing {
        AppConfig::load_from_file(&paths.config)
            .with_context(|| format!("Cannot read {}; fix it or move it aside first", paths.config.display()))?
    } else {
        AppConfig::default()
    };
    let old_endpoint = base.llm.endpoint.clone();
    let applied = omni_core::answer_file::apply(&doc, base)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("The answer file {} was not applied; nothing changed", answer.display()))?;
    let not_applied = || format!("The answer file {} was not applied; nothing changed", answer.display());
    install::check_config_watchfolder(paths, &applied.config, drives).with_context(not_applied)?;
    let endpoint = applied.config.llm.endpoint.trim().to_string();
    if !endpoint.is_empty() {
        omni_email::llm::check_base_url(&endpoint)
            .map_err(|why| anyhow::anyhow!("llm.endpoint ({endpoint}): {why}"))
            .with_context(not_applied)?;
    }
    // A new provider does not inherit the old one's key (as the LLM card).
    let same_base = |a: &str, b: &str| a.trim().trim_end_matches('/').eq_ignore_ascii_case(b.trim().trim_end_matches('/'));
    let key_dropped = if !same_base(&endpoint, &old_endpoint) && store.get_lossy(secret_keys::LLM_API_KEY).is_some_and(|k| !k.is_empty()) {
        store.remove(secret_keys::LLM_API_KEY).context("Cannot remove the LLM key of the previous provider; nothing changed")?;
        true
    } else {
        false
    };
    if existing {
        let prev = paths.config.with_extension("json.prev");
        std::fs::copy(&paths.config, &prev)
            .with_context(|| format!("Cannot keep the previous configuration as {}; nothing changed", prev.display()))?;
    }
    applied.config.save_to_file(&paths.config)?;

    let mut lines = vec![format!("✓ {} applied to {}.", answer.display(), paths.config.display())];
    if service_running {
        lines.push("! The service is running: restart it now, before anyone saves a setting in the admin panel.".into());
    }
    if existing {
        lines.push(format!("  The previous configuration is kept as {}.", paths.config.with_extension("json.prev").display()));
    }
    if key_dropped {
        lines.push("  The LLM key was removed: the answer file names another LLM provider. Enter its key on the LLM card.".into());
    }
    lines.extend(applied.notes.iter().map(|n| format!("  Note: {n}")));
    lines.push("  Secrets are not in an answer file: enter the mailbox client secret (and an LLM key) in the setup wizard.".into());
    lines.push("  Restart the service to use the new settings (omni-ingest service stop, then start).".into());
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install() -> (tempfile::TempDir, AppPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(dir.path(), "config.json");
        (dir, paths)
    }

    fn answers(dir: &Path, doc: serde_json::Value) -> std::path::PathBuf {
        let p = dir.join("omni-answers.json");
        std::fs::write(&p, doc.to_string()).unwrap();
        p
    }

    /// `Z:` is a mapped share; every other letter a local disk.
    struct Drives;
    impl omni_core::paths::DriveLookup for Drives {
        fn drive_kind(&self, letter: char) -> omni_core::paths::DriveKind {
            if letter == 'Z' { omni_core::paths::DriveKind::Remote } else { omni_core::paths::DriveKind::Other }
        }
        fn connection(&self, _letter: char) -> Option<String> {
            Some(r"\\nas-test\ingest".into())
        }
        fn persistent_mapping(&self, _letter: char) -> Option<String> {
            None
        }
    }

    fn store(paths: &AppPaths) -> SecretStore {
        SecretStore::new(paths.resolve("data/secrets.bin"))
    }

    /// The service stopped, `Z:` mapped.
    fn apply(paths: &AppPaths, file: &Path) -> Result<Vec<String>> {
        apply_answer_file_with(paths, file, &Drives, &store(paths), false, false)
    }

    #[test]
    fn an_answer_file_is_applied_over_the_existing_config_which_is_kept() {
        let (dir, paths) = install();
        let mut before = AppConfig::default();
        before.temp_path = "scratch-test".into();
        before.setup_completed = true;
        before.save_to_file(&paths.config).unwrap();
        let file = answers(dir.path(), serde_json::json!({
            "answer_file_version": 1,
            "setup_completed": false,
            "web_port": 8093,
            "watchfolder_path": r"\\dalet-test\ingest",
            "tls": { "cert_path": r"C:\certs\panel.pfx" },
        }));

        let lines = apply(&paths, &file).unwrap();
        let after = AppConfig::load_from_file(&paths.config).unwrap();
        assert_eq!((after.web_port, after.watchfolder_path.as_str()), (8093, r"\\dalet-test\ingest"));
        assert_eq!(after.temp_path, "scratch-test", "what the file does not name is kept");
        assert!(after.setup_completed, "the file never changes whether setup was finished");
        assert_eq!(after.tls.cert_path, None, "TLS is never taken from an answer file");
        assert!(lines.iter().any(|l| l.contains(omni_core::answer_file::TLS_NOTE)), "{lines:?}");
        let prev = AppConfig::load_from_file(paths.config.with_extension("json.prev")).unwrap();
        assert_eq!(prev.web_port, AppConfig::default().web_port);
    }

    const PASS: &str = "invented-passphrase-42";

    fn audit_text(repo: &Repository) -> String {
        repo.get_recent_audit_logs(1000).unwrap().iter().map(|a| a.message.clone()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn create_admin_gives_a_way_in_to_a_database_with_users_but_no_administrator() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        repo.create_user("reporter.test", "correct-horse-battery", UserRole::User, "Reporter", None).unwrap();
        assert!(!repo.has_active_admin().unwrap());

        // A weak password, the first-run login, a login in use: refused, nothing made.
        assert!(create_admin(&repo, "station.it", "short", false).is_err());
        assert!(create_admin(&repo, "admin", PASS, false).is_err());
        assert!(create_admin(&repo, "Reporter.Test", PASS, false).is_err());
        assert!(!repo.has_active_admin().unwrap());

        let line = create_admin(&repo, " Station.IT ", PASS, false).unwrap();
        let user = repo.get_user_by_email("station.it").unwrap().expect("created");
        assert_eq!(user.role, UserRole::Admin);
        assert!(user.is_active && omni_core::auth::verify_password(PASS, &user.password_hash));
        let audit = audit_text(&repo);
        assert!(audit.contains("Administrator station.it created from the command line"), "{audit}");
        assert!(!line.contains(PASS) && !audit.contains(PASS), "the password is never written out");
    }

    #[test]
    fn create_admin_is_refused_while_an_administrator_exists_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        repo.create_user("first.admin", PASS, UserRole::Admin, "First", None).unwrap();

        let err = create_admin(&repo, "second.admin", PASS, false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"), "{err:#}");
        assert!(repo.get_user_by_email("second.admin").unwrap().is_none());

        create_admin(&repo, "second.admin", PASS, true).unwrap();
        assert!(repo.get_user_by_email("second.admin").unwrap().is_some());
        assert!(!audit_text(&repo).contains(PASS));
    }

    #[test]
    fn a_bad_answer_file_changes_nothing_and_names_the_key() {
        let (dir, paths) = install();
        AppConfig::default().save_to_file(&paths.config).unwrap();
        let before = std::fs::read_to_string(&paths.config).unwrap();
        for (doc, key) in [
            (serde_json::json!({ "answer_file_version": 1, "graph": { "client_secret": "value-test-only" } }), "graph.client_secret"),
            (serde_json::json!({ "answer_file_version": 1, "mcr_open_networks": ["newsroom-lan"] }), "mcr_open_networks"),
            // P13.3b: the installer's watchfolder rules, with its text.
            (serde_json::json!({ "answer_file_version": 1, "watchfolder_path": r"Z:\ingest" }), r"use \\nas-test\ingest\ingest instead"),
            (serde_json::json!({ "answer_file_version": 1, "watchfolder_path": "" }), "No watchfolder was given"),
            (serde_json::json!({ "answer_file_version": 1, "watchfolder_path": "D:x" }), "is a relative path"),
            // P13.3b: the LLM card's rule for an online endpoint.
            (serde_json::json!({ "answer_file_version": 1, "llm": { "endpoint": "http://llm.provider.example/v1" } }), "https://"),
        ] {
            let err = apply(&paths, &answers(dir.path(), doc)).unwrap_err();
            assert!(format!("{err:#}").contains(key), "{key}: {err:#}");
            assert!(format!("{err:#}").contains("nothing changed"), "{err:#}");
        }
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), before);
        assert!(!paths.config.with_extension("json.prev").exists());
    }

    #[test]
    fn a_new_llm_provider_in_an_answer_file_drops_the_stored_key_and_the_same_one_keeps_it() {
        let (dir, paths) = install();
        let mut base = AppConfig::default();
        base.llm.endpoint = "https://llm-a.example/v1".into();
        base.save_to_file(&paths.config).unwrap();
        let key = secret_keys::LLM_API_KEY;
        store(&paths).set(key, "sk-invented-test").unwrap();
        let llm = |endpoint: &str| serde_json::json!({ "answer_file_version": 1, "llm": { "endpoint": endpoint } });

        // The same provider (case and a trailing slash aside): the key stays.
        apply(&paths, &answers(dir.path(), llm("https://LLM-A.example/v1/"))).unwrap();
        assert_eq!(store(&paths).get(key).unwrap().as_deref(), Some("sk-invented-test"));
        // A local endpoint over http is allowed; it is another provider: the key goes.
        let lines = apply(&paths, &answers(dir.path(), llm("http://127.0.0.1:1234/v1"))).unwrap();
        assert_eq!(store(&paths).get(key).unwrap(), None, "never sent to the new host");
        assert!(lines.iter().any(|l| l.contains("LLM key was removed")), "{lines:?}");
        assert!(!lines.join("\n").contains("sk-invented-test"));
    }

    #[test]
    fn while_the_service_runs_an_answer_file_is_refused_unless_forced() {
        let (dir, paths) = install();
        AppConfig::default().save_to_file(&paths.config).unwrap();
        let before = std::fs::read_to_string(&paths.config).unwrap();
        let file = answers(dir.path(), serde_json::json!({ "answer_file_version": 1, "web_port": 8095 }));
        let err = apply_answer_file_with(&paths, &file, &Drives, &store(&paths), true, false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), before);
        let lines = apply_answer_file_with(&paths, &file, &Drives, &store(&paths), true, true).unwrap();
        assert!(lines.iter().any(|l| l.contains("restart it now")), "{lines:?}");
        assert_eq!(AppConfig::load_from_file(&paths.config).unwrap().web_port, 8095);
    }
}
