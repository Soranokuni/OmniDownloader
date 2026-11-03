//! `omni-ingest install` / `uninstall`: one command from a release folder to
//! a running service (plan P9).
//!
//! The release folder (made by `scripts/package.ps1`) holds this executable,
//! the broadcast tools in `bin/`, and `Install.cmd`. Running the install:
//!
//! 1. copies the executable and the tools to the install directory (default
//!    `C:\OmniIngest`; not Program Files: the service writes its database,
//!    logs and work files next to itself), keeping the previous executable as
//!    `omni-ingest.exe.prev`;
//! 2. writes `config.json` only when there is none, asking for the Dalet
//!    watchfolder; an existing configuration is never rewritten except for
//!    what was passed on the command line;
//! 3. registers the Windows service (automatic, delayed start; restarts by
//!    itself after a failure), opens the panel's port in the firewall, and
//!    starts it;
//! 4. waits for `/api/health` and prints what the service reports. An
//!    upgrade that does not come up puts the previous executable back.
//!
//! Run it again from a newer release folder to upgrade: data, configuration,
//! secrets and the self-updating yt-dlp and Deno are left alone.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use inquire::Text;

use omni_core::config::AppConfig;
use omni_core::paths::AppPaths;
use omni_service::{
    can_install_services, default_or_given, install_service, installed_service_account, installed_service_exe,
    needs_network_identity, network_share_warning_applies, start_service, stop_service_and_wait, uninstall_service,
    AccountChoice, ServiceAccount, DEFAULT_SERVICE_ACCOUNT,
};

pub const DEFAULT_DIR: &str = r"C:\OmniIngest";
const EXE: &str = "omni-ingest.exe";
/// The broadcast tools the release carries. Replaced on upgrade when the
/// release's copy differs.
const TOOLS: &[&str] = &["ffmpeg.exe", "ffprobe.exe", "bmxtranswrap.exe"];
/// Tools that update themselves nightly (plan P2.9/P2.10/P6.8): installed
/// from the release only when missing, never replaced by an older copy.
const SELF_UPDATING: &[&str] = &["yt-dlp", "yt-dlp.exe", "deno.exe"];
/// Seeds read once on first start (roster, taxonomy): copied when missing.
const SEEDS: &[&str] = &["journalists.seed.json", "taxonomy.json"];
const FIREWALL_RULE: &str = "OmniIngest web panel";
/// First start can install the yt-dlp folder build and Deno, and runs the
/// encoder self-test before the web server answers.
const HEALTH_WAIT: Duration = Duration::from_secs(240);

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub dir: Option<PathBuf>,
    pub watchfolder: Option<String>,
    pub port: Option<u16>,
    pub account: Option<String>,
    /// Ask nothing; take defaults.
    pub yes: bool,
    pub no_firewall: bool,
    /// An existing installation (or a dev checkout) whose configuration,
    /// database, secrets and seeds move into this new one.
    pub import: Option<PathBuf>,
}

/// Bring an older installation's state into a new, empty one: its
/// config.json, data\omni.db, data\secrets.bin (machine-scope DPAPI: valid on
/// this PC only), seeds and adblock lists. Paths in its config that were
/// relative to the old folder and point at things that stay there (the
/// watchfolder) are made absolute, so they still point where they did; the
/// database, temp and archive paths stay relative and follow the move.
fn import_from(old: &Path, new: &AppPaths) -> Result<AppConfig> {
    let old_paths = AppPaths::with_root(old, "config.json");
    if !old_paths.config.is_file() {
        bail!("{} has no config.json to import", old.display());
    }
    if new.config.exists() || new.data.join("omni.db").exists() {
        bail!("{} already has a configuration or a database; import only into a new installation", new.root.display());
    }
    let mut config = AppConfig::load_from_file(&old_paths.config).context("The old config.json cannot be read")?;
    let db = old_paths.resolve(&config.database_path);
    let wal = db.with_extension("db-wal");
    if std::fs::metadata(&wal).map(|m| m.len() > 0).unwrap_or(false) {
        bail!(
            "{} is still in use (its write-ahead log is not empty). Stop the old OmniDownloader first \
             (close its console window or stop its service), then run the installer again.",
            db.display()
        );
    }
    config.watchfolder_path = absolute(&old_paths, &config.watchfolder_path);
    std::fs::create_dir_all(&new.data)?;
    std::fs::copy(&db, new.data.join("omni.db")).with_context(|| format!("Cannot copy {}", db.display()))?;
    config.database_path = "data/omni.db".into();
    for f in ["secrets.bin", "journalists.seed.json", "taxonomy.json"] {
        let src = old_paths.data.join(f);
        if src.is_file() {
            std::fs::copy(&src, new.data.join(f))?;
        }
    }
    if old_paths.data.join("adblock").is_dir() {
        copy_recursive(&old_paths.data.join("adblock"), &new.data.join("adblock"))?;
    }
    println!("  Imported the configuration, database, secrets and seeds from {}.", old.display());
    println!("  Watchfolder stays {}.", config.watchfolder_path);
    Ok(config)
}

/// Refuse a watchfolder the service cannot use (a mapped drive letter, a path that is
/// empty), with the path to use instead. A relative value in config.json is legitimate
/// (it means "under the install folder"), so the resolved path is judged, never the raw one.
pub(crate) fn check_config_watchfolder(
    paths: &AppPaths,
    config: &AppConfig,
    drives: &dyn omni_core::paths::DriveLookup,
) -> Result<()> {
    let raw = config.watchfolder_path.trim();
    let judged = if raw.is_empty() { String::new() } else { paths.resolve(raw).to_string_lossy().into_owned() };
    match omni_core::paths::watchfolder_problem_with(&judged, drives) {
        Some(problem) => anyhow::bail!("Cannot install: {problem}"),
        None => Ok(()),
    }
}

/// A config path as an absolute one, resolved against the folder it was
/// relative to; UNC and absolute paths unchanged.
fn absolute(paths: &AppPaths, p: &str) -> String {
    paths.resolve(p).display().to_string()
}

pub async fn install(o: InstallOptions) -> Result<()> {
    if !can_install_services() {
        bail!(
            "The installer needs administrator rights. Run Install.cmd (it asks for them), \
             or run this from an elevated terminal."
        );
    }
    let source_exe = std::env::current_exe().context("Cannot find this executable")?;
    let source = source_exe.parent().context("No release folder")?.to_path_buf();
    let target = o.dir.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    std::fs::create_dir_all(&target).with_context(|| format!("Cannot create {}", target.display()))?;
    let paths = AppPaths::with_root(&target, "config.json");
    let in_place = same_dir(&source, &paths.root);
    let target_exe = paths.root.join(EXE);
    let service_exe = installed_service_exe();
    let upgrade = target_exe.exists() || service_exe.is_some();

    println!();
    println!("OmniDownloader {} → {}", env!("CARGO_PKG_VERSION"), paths.root.display());
    println!("  {}", if upgrade { "Upgrading the existing installation." } else { "New installation." });

    // ---- configuration (decided before anything is stopped) --------------
    let imported = match &o.import {
        Some(old) => Some(import_from(old, &paths)?),
        None => None,
    };
    let fresh_config = !paths.config.exists();
    let mut config = if let Some(c) = imported.clone() {
        c
    } else if fresh_config {
        AppConfig::default()
    } else {
        AppConfig::load_from_file(&paths.config).context("The existing config.json cannot be read")?
    };
    if let Some(w) = &o.watchfolder {
        config.watchfolder_path = w.clone();
    } else if imported.is_some() {
        // The old installation's watchfolder, made absolute by the import.
    } else if fresh_config && !o.yes {
        let default = paths.root.join("watchfolder").display().to_string();
        let answer = Text::new("Dalet watchfolder (local folder or \\\\server\\share):")
            .with_default(&default)
            .prompt()
            .context("No watchfolder given")?;
        config.watchfolder_path = answer.trim().to_string();
    } else if fresh_config {
        config.watchfolder_path = paths.root.join("watchfolder").display().to_string();
    }
    if let Some(p) = o.port {
        config.web_port = p;
    }
    // Before anything is stopped or copied. A relative value in config.json is
    // legitimate (it means "under the install folder"), so judge the resolved path.
    check_config_watchfolder(&paths, &config, &omni_core::paths::SystemDrives)?;

    // The service is (re)registered when it is missing, points elsewhere, or
    // a new account was asked for. Otherwise its account is kept as it is.
    let previous_account = installed_service_account();
    let decision = account_plan(service_exe.as_deref(), &target_exe, o.account.as_deref(), previous_account.as_deref());
    let register = decision.register;
    let account = if register {
        choose_account(o.account.as_deref(), &config.watchfolder_path, o.yes)?
    } else {
        // An upgrade never changes the account by itself; say how to switch.
        if decision.switch_hint {
            println!("  The service runs as LocalSystem (kept). To switch: Install.cmd --account \"{DEFAULT_SERVICE_ACCOUNT}\"");
        }
        None
    };

    // ---- stop, copy -------------------------------------------------------
    if service_exe.is_some() {
        println!("  Stopping the service…");
        stop_service_and_wait(Duration::from_secs(120)).context("The running service would not stop")?;
    }
    let had_previous = if in_place {
        false
    } else {
        copy_executable(&source_exe, &target_exe)?
    };
    if !in_place {
        copy_tools(&source.join("bin"), &paths.bin)?;
        copy_missing(&source.join("data"), &paths.data, SEEDS)?;
    }
    paths.ensure_dirs().context("Cannot create the data, temp and log folders")?;
    let watch = paths.resolve(&config.watchfolder_path);
    if !needs_network_identity(&config.watchfolder_path) {
        let _ = std::fs::create_dir_all(&watch);
    }
    if fresh_config || o.watchfolder.is_some() || o.port.is_some() {
        config.save_to_file(&paths.config).context("Cannot write config.json")?;
        println!("  Configuration written to {}", paths.config.display());
    }

    // ---- service, rights, firewall -----------------------------------------
    if register {
        if service_exe.is_some() {
            uninstall_service().context("Cannot remove the old service registration")?;
        }
        install_service(&target_exe, account.as_ref()).context("Cannot register the service")?;
        println!(
            "  Service registered: automatic (delayed) start, restarts after a failure, runs as {}.",
            account_label(account.as_ref())
        );
        apply_acls(&paths, &config, account.as_ref(), previous_account.as_deref(), &target_exe).await;
    }
    if !o.no_firewall && !is_loopback(&config.web_host) {
        open_firewall(&target_exe, config.web_port).await;
    }

    // ---- start and verify --------------------------------------------------
    println!("  Starting the service and waiting for it to answer (first start can take a few minutes)…");
    start_service().context("The service would not start")?;
    let probe = health_address(&config);
    match wait_for_health(probe, config.tls.is_enabled(), HEALTH_WAIT) {
        Some(report) => {
            println!();
            println!("✓ OmniDownloader is running.");
            print_health(&report);
        }
        None if upgrade && had_previous => {
            println!();
            println!("✗ The new version did not answer within {} s. Putting the previous one back…", HEALTH_WAIT.as_secs());
            let _ = stop_service_and_wait(Duration::from_secs(120));
            std::fs::copy(target_exe.with_extension("exe.prev"), &target_exe).context("Rollback failed")?;
            start_service().context("The previous version would not start either")?;
            bail!(
                "Upgrade rolled back; the previous version is running. See {} for why the new one failed.",
                paths.logs.display()
            );
        }
        None => {
            bail!(
                "The service did not answer on {probe} within {} s. See {} and Event Viewer → Windows Logs → Application.",
                HEALTH_WAIT.as_secs(),
                paths.logs.display()
            );
        }
    }

    // ---- what next ---------------------------------------------------------
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this-pc".into());
    let scheme = if config.tls.is_enabled() { "https" } else { "http" };
    println!();
    println!("  MCR desk:  {scheme}://{host}:{}/mcr", config.web_port);
    println!("  Admin:     {scheme}://{host}:{}/admin", config.web_port);
    println!("  Files:     {}", paths.root.display());
    println!("  Watchfolder: {}", watch.display());
    if upgrade {
        println!("  Previous version kept as {}.", target_exe.with_extension("exe.prev").display());
    } else {
        println!();
        println!("  Next:");
        println!("   1. On this server only, open {scheme}://127.0.0.1:{}/admin and log in as  Admin / Admin.", config.web_port);
        println!("      The page asks for a new password first; from any other PC Admin / Admin is refused.");
        println!("   2. Users: create your own administrator, log in as it, and deactivate Admin.");
        println!("   3. Set the mailbox (Microsoft Graph) and, if wanted, the LLM key.");
        println!("   4. Security: add the newsroom network so the MCR desks can open the panel.");
    }
    Ok(())
}

/// Remove the service and the firewall rule; leave every file in place.
pub async fn uninstall(dir: Option<PathBuf>) -> Result<()> {
    if !can_install_services() {
        bail!("Uninstalling needs administrator rights (run Uninstall.cmd or an elevated terminal).");
    }
    if installed_service_exe().is_some() {
        let _ = stop_service_and_wait(Duration::from_secs(120));
        uninstall_service()?;
        println!("✓ Service removed.");
    } else {
        println!("  The service was not installed.");
    }
    let _ = netsh(&["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).await;
    let dir = dir.unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    println!(
        "  The files are still in {} (database, configuration, logs). Delete that folder to remove them too.",
        dir.display()
    );
    Ok(())
}

/// Decide the service account (P2.16: the virtual account unless told
/// otherwise), warn about a network watchfolder it cannot reach, and ask for a
/// password only when the account needs one. `None` = LocalSystem.
///
/// The share warning is for the defaulted account only: an operator who named
/// `LocalSystem` or a built-in account on the command line chose it knowingly.
pub(crate) fn choose_account(account: Option<&str>, watchfolder: &str, yes: bool) -> Result<Option<ServiceAccount>> {
    let choice = default_or_given(account);
    let defaulted = account.map_or(true, |s| s.trim().is_empty());
    if defaulted && network_share_warning_applies(watchfolder, &choice) {
        let who = match &choice {
            AccountChoice::Passwordless(n) | AccountChoice::NeedsPassword(n) => n.as_str(),
            AccountChoice::LocalSystem => "LocalSystem",
        };
        println!();
        println!("! The watchfolder {watchfolder} is a network share. As {who} the service");
        println!("  reaches it as this computer's account, which most file servers refuse; files");
        println!("  then fail at the very last step. Either install with a domain account that");
        println!("  has Modify on the share:   Install.cmd --account \"DOMAIN\\svc_omni\"");
        println!("  or grant this computer's account (DOMAIN\\THIS-PC$) Modify on the share.");
        if yes {
            bail!("A network watchfolder needs --account (a domain account), or the computer account granted on the share.");
        }
        let go = inquire::Confirm::new(&format!("Install as {who} anyway?")).with_default(false).prompt()?;
        if !go {
            bail!("Installation cancelled.");
        }
    }
    match choice {
        AccountChoice::LocalSystem => Ok(None),
        AccountChoice::Passwordless(name) => Ok(Some(ServiceAccount { name, password: None })),
        AccountChoice::NeedsPassword(name) => {
            let password = inquire::Password::new(&format!("Password for {name}:"))
                .without_confirmation()
                .prompt()
                .context("No password given")?;
            Ok(Some(ServiceAccount::with_password(name, password)))
        }
    }
}

/// How the service account is told to the operator.
fn account_label(account: Option<&ServiceAccount>) -> &str {
    account.map(|a| a.name.as_str()).unwrap_or("LocalSystem")
}

/// Which rights the installer gives the service account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rights {
    /// The install root: inheritance cut, SYSTEM and Administrators Full, Users
    /// read-only (`lockdown::root_lockdown_args`). Never a write right.
    LockRoot,
    /// Modify, inherited by everything inside (`(OI)(CI)M`).
    ModifyDir,
    /// Modify on this one file (`M`).
    ModifyFile,
    /// Authenticated Users Modify on a watchfolder inside the root, which is
    /// what it inherited from `C:\` before the root was locked: Dalet's ingest
    /// account keeps its access. The owner may tighten it later.
    ModifyAuthenticatedUsers,
}

/// `*S-1-5-11`, how icacls takes Authenticated Users.
const AUTHENTICATED_USERS: &str = "*S-1-5-11";

/// Whether `path` is inside `root` (Windows paths compare without case).
fn is_under(path: &Path, root: &Path) -> bool {
    let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_ascii_lowercase());
    lower(path).starts_with(lower(root))
}

/// The ACL steps after the old grants are stripped, in order. The root is
/// locked first (so `C:\`'s inherited `Authenticated Users:Modify` is gone
/// before anything is granted). A local watchfolder under the root keeps
/// Authenticated Users Modify, LocalSystem install included. For a named
/// service account, Modify on what the daemon writes and nothing else: the root
/// itself gets no write right, so a local user cannot replace the executable.
/// Every path comes from the config the way the daemon resolves it. A UNC
/// watchfolder is not ours to grant.
pub(crate) fn grant_plan(paths: &AppPaths, config: &AppConfig, named_account: bool) -> Vec<(PathBuf, Rights)> {
    let mut plan: Vec<(PathBuf, Rights)> = Vec::new();
    let mut add = |p: PathBuf, r: Rights| {
        if !plan.contains(&(p.clone(), r)) {
            plan.push((p, r));
        }
    };
    add(paths.root.clone(), Rights::LockRoot);
    let watch = paths.resolve(&config.watchfolder_path);
    let local_watch = !needs_network_identity(&config.watchfolder_path);
    if local_watch && is_under(&watch, &paths.root) {
        add(watch.clone(), Rights::ModifyAuthenticatedUsers);
    }
    if named_account {
        let db_dir = paths.resolve(&config.database_path).parent().map(Path::to_path_buf);
        add(paths.data.clone(), Rights::ModifyDir);
        if let Some(d) = db_dir {
            add(d, Rights::ModifyDir);
        }
        add(paths.resolve(&config.temp_path), Rights::ModifyDir);
        add(paths.logs.clone(), Rights::ModifyDir);
        add(paths.resolve(&config.archive.path), Rights::ModifyDir);
        add(paths.resolve(&config.bin_dir), Rights::ModifyDir);
        add(paths.config.clone(), Rights::ModifyFile);
        if local_watch {
            add(watch, Rights::ModifyDir);
        }
    }
    plan
}

/// The icacls grant spec: containers and objects inherit only for a folder.
fn grant_spec(account: &str, rights: Rights) -> String {
    match rights {
        Rights::LockRoot => unreachable!("the root is locked by lockdown::lock_root"),
        Rights::ModifyDir | Rights::ModifyAuthenticatedUsers => format!("{account}:(OI)(CI)M"),
        Rights::ModifyFile => format!("{account}:M"),
    }
}

/// What an install run does about the service registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccountPlan {
    /// Register the service (again).
    pub register: bool,
    /// Tell the operator how to move a kept LocalSystem service to the virtual account.
    pub switch_hint: bool,
}

/// The service is (re)registered when it is missing, points elsewhere, or a new
/// account was asked for. Otherwise its account is kept as it is, and a kept
/// LocalSystem one gets a hint. Pure; `current_account` is what the installed
/// service runs as.
pub(crate) fn account_plan(
    installed_exe: Option<&Path>,
    target_exe: &Path,
    account_arg: Option<&str>,
    current_account: Option<&str>,
) -> AccountPlan {
    let asked = account_arg.map_or(false, |s| !s.trim().is_empty());
    let register = installed_exe.map_or(true, |p| !same_file(p, target_exe)) || asked;
    let switch_hint = !register
        && current_account.map_or(false, |c| matches!(default_or_given(Some(c)), AccountChoice::LocalSystem));
    AccountPlan { register, switch_hint }
}

/// All of the ACL work of an install, shared by the installer, `service install`
/// and the wizard (a service registered without it would not start on a tree
/// that ran as LocalSystem: data\ and temp\ are then SYSTEM-only). Order:
/// strip the old explicit grants (the previous and the new account), lock the
/// root, grant, then lock data\ and temp\. Failures print and carry on, except
/// the data and temp grants of a named account, which say the service will not start.
pub(crate) async fn apply_acls(
    paths: &AppPaths,
    config: &AppConfig,
    account: Option<&ServiceAccount>,
    previous_account: Option<&str>,
    // The executable the service is registered with.
    service_exe: &Path,
) {
    let named = account.map(|a| a.name.as_str());
    let temp = paths.resolve(&config.temp_path);
    // A development tree (the exe in `target\release`, the root the repo) is never locked: it
    // would take write access from the developer's own non-elevated account (UAC).
    let lock_root = is_installed_root(service_exe, &paths.root);
    if !lock_root {
        println!("  {} is not an installed root (development tree); not restricting it.", paths.root.display());
    }
    // 1. An older installer granted the account Modify on the whole root and every file.
    let mut stale: Vec<&str> = Vec::new();
    for a in [previous_account, named].into_iter().flatten() {
        if !stale.iter().any(|s| s.eq_ignore_ascii_case(a)) {
            stale.push(a);
        }
    }
    for a in stale.into_iter().filter(|_| lock_root) {
        if let Err(e) = omni_core::lockdown::remove_grants(&paths.root, a).await {
            println!("! Could not remove the old rights of {a} ({e:#}); set it by hand.");
        }
    }
    // 2. Root, then the folders the daemon writes.
    let mut service_blocked = false;
    for (path, rights) in grant_plan(paths, config, named.is_some()) {
        match (rights, named) {
            (Rights::LockRoot, _) if !lock_root => {}
            (Rights::LockRoot, _) => match omni_core::lockdown::lock_root(&path, named).await {
                Ok(()) => println!("  Install folder restricted: Users may read, only SYSTEM and Administrators may change it."),
                Err(e) => println!("! Could not restrict the install folder ({e:#}); set it by hand."),
            },
            (Rights::ModifyAuthenticatedUsers, _) => {
                grant_modify(AUTHENTICATED_USERS, &path, Rights::ModifyDir).await;
            }
            (_, Some(name)) => {
                let ok = grant_modify(name, &path, rights).await;
                if !ok && (path == paths.data || path == temp) {
                    service_blocked = true;
                }
            }
            _ => {}
        }
    }
    if service_blocked {
        println!("! The service will NOT start: {} has no access to data\\ or temp\\. Grant it Modify there, then start the service.", account_label(account));
    }
    // 3. data\ and temp\ hold the sealed secrets and cookie jars (P3.5): after the
    //    grants, so the service account keeps Modify on them.
    let account_arg = match account {
        Some(a) => omni_core::lockdown::Account::Named(a.name.clone()),
        None => omni_core::lockdown::Account::System,
    };
    let dirs = [paths.data.clone(), temp];
    match omni_core::lockdown::apply(&dirs, account_arg).await {
        Ok(()) => println!("  data and temp folders restricted to SYSTEM, Administrators and the service account."),
        Err(e) => println!("! Could not restrict the data and temp folders ({e:#}); set it by hand."),
    }
}

/// Replace the installed executable with this one, keeping the old one as
/// `.prev`. Returns whether there was an old one.
fn copy_executable(source: &Path, target: &Path) -> Result<bool> {
    let prev = target.with_extension("exe.prev");
    let had = target.exists();
    if had {
        std::fs::copy(target, &prev).with_context(|| format!("Cannot keep the previous version as {}", prev.display()))?;
    }
    let staged = target.with_extension("exe.new");
    std::fs::copy(source, &staged).with_context(|| format!("Cannot copy to {}", staged.display()))?;
    std::fs::rename(&staged, target).with_context(|| format!("Cannot install {}", target.display()))?;
    println!("  Installed {}.", target.display());
    Ok(had)
}

/// The release's tools into `bin/`: replaced when different, self-updating
/// ones only when missing, DLLs alongside.
fn copy_tools(from: &Path, to: &Path) -> Result<()> {
    if !from.is_dir() {
        println!("  (No bin/ in this release folder: tools left as they are.)");
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let src = entry.path();
        let dst = to.join(&name);
        let is_tool = TOOLS.contains(&name.as_str()) || name.to_ascii_lowercase().ends_with(".dll");
        if SELF_UPDATING.contains(&name.as_str()) {
            if !dst.exists() {
                copy_recursive(&src, &dst)?;
                println!("  Installed bin\\{name}.");
            }
        } else if is_tool && differs(&src, &dst) {
            std::fs::copy(&src, &dst).with_context(|| format!("Cannot copy {name}"))?;
            println!("  Installed bin\\{name}.");
        }
    }
    Ok(())
}

fn copy_missing(from: &Path, to: &Path, names: &[&str]) -> Result<()> {
    for name in names {
        let src = from.join(name);
        let dst = to.join(name);
        if src.is_file() && !dst.exists() {
            std::fs::create_dir_all(to)?;
            std::fs::copy(&src, &dst)?;
            println!("  Installed data\\{name}.");
        }
    }
    Ok(())
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst).with_context(|| format!("Cannot copy {}", src.display()))?;
    }
    Ok(())
}

fn differs(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {
            let (Ok(x), Ok(y)) = (std::fs::read(a), std::fs::read(b)) else { return true };
            omni_core::dependencies::sha256_hex(&x) != omni_core::dependencies::sha256_hex(&y)
        }
        _ => true,
    }
}

/// Whether `root` is an installed root: the service's executable sits directly in it. A cargo
/// build (`<repo>\target\release\omni-ingest.exe`, root `<repo>`) is not.
pub(crate) fn is_installed_root(service_exe: &Path, root: &Path) -> bool {
    // Nor any other cargo output (`target\gate\release`, `target\<triple>\release`): a folder named
    // `target`, or a `Cargo.toml`, at or above the root.
    let in_cargo_tree = root
        .ancestors()
        .any(|a| a.file_name().is_some_and(|n| n.eq_ignore_ascii_case("target")) || a.join("Cargo.toml").is_file());
    !in_cargo_tree && service_exe.parent().is_some_and(|dir| same_dir(dir, root))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (dunce(a), dunce(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    same_dir(a, b) || a.to_string_lossy().trim_matches('"').eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Canonical, case-folded, without the `\\?\` prefix Windows adds.
fn dunce(p: &Path) -> Option<String> {
    let c = std::fs::canonicalize(p).ok()?;
    Some(c.to_string_lossy().trim_start_matches(r"\\?\").to_ascii_lowercase())
}

/// The service account gets Modify on what it writes.
async fn grant_modify(account: &str, path: &Path, rights: Rights) -> bool {
    if rights == Rights::ModifyDir {
        let _ = std::fs::create_dir_all(path);
    }
    let args = [path.as_os_str().to_os_string(), "/grant".into(), grant_spec(account, rights).into(), "/T".into(), "/Q".into()];
    match omni_core::process::run_capture(Path::new("icacls"), args, Duration::from_secs(120)).await {
        Ok(o) if o.success => {
            println!("  {account} may modify {}.", path.display());
            true
        }
        _ => {
            println!("! Could not give {account} Modify on {}; set it by hand.", path.display());
            false
        }
    }
}

async fn netsh(args: &[&str]) -> bool {
    matches!(
        omni_core::process::run_capture(Path::new("netsh"), args, Duration::from_secs(30)).await,
        Ok(o) if o.success
    )
}

/// One inbound rule for this executable on the panel's port, on the domain
/// and private networks (not public Wi-Fi). Replaced on every install.
async fn open_firewall(exe: &Path, port: u16) {
    let _ = netsh(&["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).await;
    let ok = netsh(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &format!("name={FIREWALL_RULE}"),
        "dir=in",
        "action=allow",
        "protocol=TCP",
        &format!("localport={port}"),
        &format!("program={}", exe.display()),
        "profile=domain,private",
    ])
    .await;
    if ok {
        println!("  Firewall: TCP {port} open on domain and private networks.");
    } else {
        println!("! Could not add the firewall rule; the desks may not reach port {port}.");
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host.trim(), "127.0.0.1" | "localhost" | "::1")
}

/// Where to ask: the bound address, or loopback when bound to all.
fn health_address(config: &AppConfig) -> SocketAddr {
    let host = config.web_host.trim();
    let ip = match host.parse::<std::net::IpAddr>() {
        Ok(ip) if !ip.is_unspecified() => ip,
        _ => std::net::IpAddr::from([127, 0, 0, 1]),
    };
    SocketAddr::new(ip, config.web_port)
}

/// Poll `/api/health` until it answers (any HTTP status: 503 is a service
/// that runs and reports a fault). With TLS on, an accepted connection is
/// the answer. `None` on timeout.
fn wait_for_health(addr: SocketAddr, tls: bool, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            if tls {
                return Some(String::new());
            }
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let req = format!("GET /api/health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            if s.write_all(req.as_bytes()).is_ok() {
                let mut buf = String::new();
                if s.read_to_string(&mut buf).is_ok() && buf.starts_with("HTTP/") {
                    return Some(buf.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default());
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    None
}

fn print_health(body: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    println!("  Health: {}", v["status"].as_str().unwrap_or("?"));
    if let Some(checks) = v["checks"].as_object() {
        for (name, state) in checks {
            let s = state.as_str().unwrap_or("?");
            let mark = if s == "ok" { "✓" } else { "!" };
            println!("    {mark} {name}: {s}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MappedZ;
    impl omni_core::paths::DriveLookup for MappedZ {
        fn drive_kind(&self, l: char) -> omni_core::paths::DriveKind {
            if l == 'Z' { omni_core::paths::DriveKind::Remote } else { omni_core::paths::DriveKind::Other }
        }
        fn connection(&self, _: char) -> Option<String> {
            Some(r"\\192.168.4.42\FileIngest".into())
        }
        fn persistent_mapping(&self, _: char) -> Option<String> {
            None
        }
    }

    #[test]
    fn install_refuses_a_mapped_drive_and_names_the_share() {
        let paths = AppPaths::with_root(r"C:\omni", "config.json");
        let check = |w: &str| {
            let mut c = AppConfig::default();
            c.watchfolder_path = w.to_string();
            check_config_watchfolder(&paths, &c, &MappedZ)
        };
        let err = check(r"Z:\Dalet").unwrap_err().to_string();
        assert!(err.contains(r"use \\192.168.4.42\FileIngest\Dalet instead"), "{err}");
        assert!(check(r"D:\Dalet").is_ok());
        assert!(check(r"\\nas\ingest").is_ok());
        // The default relative value means "under the install folder": fine.
        assert!(check("watchfolder").is_ok());
        assert!(check("  ").is_err());
    }

    /// The root is restricted only when the service's exe sits directly in it: never a repo whose
    /// build is in `target\release` (the developer would lose write access under UAC).
    #[test]
    fn only_an_installed_root_is_restricted_never_a_development_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("OmniIngest");
        let release = dir.path().join("repo").join("target").join("release");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&release).unwrap();
        assert!(is_installed_root(&root.join("omni-ingest.exe"), &root));
        let repo = dir.path().join("repo");
        assert!(!is_installed_root(&release.join("omni-ingest.exe"), &repo), "a cargo build's root is the repo");
        assert!(!is_installed_root(&root.join("sub").join("omni-ingest.exe"), &root));
        // Another cargo output, where the discovered root is the profile folder itself.
        let gate = dir.path().join("repo").join("target").join("gate").join("release");
        std::fs::create_dir_all(&gate).unwrap();
        assert!(!is_installed_root(&gate.join("omni-ingest.exe"), &gate), "target\\gate\\release is a build, not an install");
        // A checkout (Cargo.toml) whose exe was copied to its top.
        std::fs::write(repo.join("Cargo.toml"), "").unwrap();
        assert!(!is_installed_root(&repo.join("omni-ingest.exe"), &repo));
    }

    #[test]
    fn the_grants_cover_what_the_daemon_writes_and_never_the_root() {
        let root = PathBuf::from(r"C:\OmniIngestTest");
        let paths = AppPaths::with_root(&root, "config.json");
        let watch = PathBuf::from(r"C:\Dalet\ingest");
        let mut config = AppConfig::default();
        config.watchfolder_path = watch.display().to_string();
        config.temp_path = "temp".into();
        config.bin_dir = "bin".into();
        config.archive.path = "archive".into();
        config.database_path = r"data\omni.db".into();
        let plan = grant_plan(&paths, &config, true);
        assert_eq!(plan[0], (root.clone(), Rights::LockRoot), "the root is locked before anything is granted");
        for d in [&paths.data, &paths.temp, &paths.logs, &paths.archive, &paths.bin, &watch] {
            assert!(plan.contains(&(d.clone(), Rights::ModifyDir)), "no Modify on {d:?}");
        }
        assert!(plan.contains(&(paths.config.clone(), Rights::ModifyFile)), "config.json is a file grant");
        assert!(
            plan.iter().all(|(p, r)| *p != root || *r == Rights::LockRoot),
            "the install root gets the lockdown, never a Modify grant"
        );
        assert!(plan.iter().all(|(p, _)| p.file_name().map_or(true, |n| n != "omni-ingest.exe")));
        // A network watchfolder is not ours to grant.
        config.watchfolder_path = r"\\dalet\ingest".into();
        let remote = grant_plan(&paths, &config, true);
        assert!(remote.iter().all(|(p, _)| p != Path::new(r"\\dalet\ingest")));
        assert_eq!(remote.len(), 7);
        // LocalSystem with a watchfolder outside the root: the root is locked, nothing else.
        config.watchfolder_path = watch.display().to_string();
        assert_eq!(grant_plan(&paths, &config, false), vec![(root.clone(), Rights::LockRoot)]);
    }

    #[test]
    fn a_watchfolder_inside_the_root_keeps_authenticated_users_modify_outside_it_does_not() {
        let root = PathBuf::from(r"C:\OmniIngestTest");
        let paths = AppPaths::with_root(&root, "config.json");
        let mut config = AppConfig::default();
        config.watchfolder_path = "watchfolder".into();
        let inside = root.join("watchfolder");
        let au = (inside.clone(), Rights::ModifyAuthenticatedUsers);
        for named in [true, false] {
            let plan = grant_plan(&paths, &config, named);
            let lock = plan.iter().position(|e| e.1 == Rights::LockRoot).unwrap();
            let pos = plan.iter().position(|e| *e == au).expect("under the root: Authenticated Users Modify");
            assert!(lock < pos, "after the root lock, or the lock would remove it");
        }
        config.watchfolder_path = r"D:\Dalet\ingest".into();
        for named in [true, false] {
            assert!(grant_plan(&paths, &config, named).iter().all(|e| e.1 != Rights::ModifyAuthenticatedUsers));
        }
        // A sibling folder that only shares the name prefix is not inside.
        config.watchfolder_path = r"C:\OmniIngestTest2\w".into();
        assert!(grant_plan(&paths, &config, false).iter().all(|e| e.1 != Rights::ModifyAuthenticatedUsers));
    }

    #[test]
    fn the_grants_follow_the_configured_folders_like_the_daemon_resolves_them() {
        let root = PathBuf::from(r"C:\OmniIngestTest");
        let paths = AppPaths::with_root(&root, "config.json");
        let mut config = AppConfig::default();
        config.watchfolder_path = r"D:\w".into();
        config.temp_path = "work".into();
        config.bin_dir = "tools".into();
        config.archive.path = r"media\arch".into();
        config.database_path = r"db\omni.db".into();
        let plan = grant_plan(&paths, &config, true);
        for d in [root.join("work"), root.join("tools"), root.join("media").join("arch"), root.join("db")] {
            assert!(plan.contains(&(d.clone(), Rights::ModifyDir)), "no Modify on {d:?}");
        }
    }

    #[test]
    fn an_existing_service_keeps_its_account_unless_one_is_asked_for() {
        let target = PathBuf::from(r"C:\OmniIngestTest\omni-ingest.exe");
        let elsewhere = PathBuf::from(r"D:\Old\omni-ingest.exe");
        let sys = Some("LocalSystem");
        // Same exe, no --account: kept, not re-registered; the hint only for LocalSystem.
        assert_eq!(account_plan(Some(&target), &target, None, sys), AccountPlan { register: false, switch_hint: true });
        assert_eq!(account_plan(Some(&target), &target, Some(" "), sys), AccountPlan { register: false, switch_hint: true });
        assert_eq!(
            account_plan(Some(&target), &target, None, Some(r"NT SERVICE\OmniIngestService")),
            AccountPlan { register: false, switch_hint: false }
        );
        assert_eq!(account_plan(Some(&target), &target, None, Some(r"DOMAIN\svc")), AccountPlan { register: false, switch_hint: false });
        // --account re-registers, no hint.
        assert_eq!(account_plan(Some(&target), &target, Some("LocalSystem"), sys), AccountPlan { register: true, switch_hint: false });
        // Missing, or pointing elsewhere: registers.
        assert!(account_plan(None, &target, None, None).register);
        assert!(account_plan(Some(&elsewhere), &target, None, sys).register);
        assert!(!account_plan(Some(&elsewhere), &target, None, sys).switch_hint);
    }

    #[test]
    fn a_file_grant_has_no_inheritance_flags() {
        assert_eq!(grant_spec(r"NT SERVICE\X", Rights::ModifyFile), r"NT SERVICE\X:M");
        assert_eq!(grant_spec(r"NT SERVICE\X", Rights::ModifyDir), r"NT SERVICE\X:(OI)(CI)M");
        assert!(!grant_spec("a", Rights::ModifyFile).contains("(OI)"));
    }

    #[test]
    fn the_account_decision_defaults_to_the_virtual_account_without_prompting() {
        let a = choose_account(None, r"D:\watchfolder", true).unwrap().unwrap();
        assert_eq!(a.name, DEFAULT_SERVICE_ACCOUNT);
        assert!(a.password.is_none());
        assert!(choose_account(Some("LocalSystem"), r"D:\watchfolder", true).unwrap().is_none());
        // A network share with the defaulted account refuses to go on under --yes.
        assert!(choose_account(None, r"\\dalet\ingest", true).is_err());
    }

    #[test]
    fn the_health_probe_goes_where_the_panel_listens() {
        let mut c = AppConfig::default();
        c.web_port = 8091;
        c.web_host = "0.0.0.0".into();
        assert_eq!(health_address(&c).to_string(), "127.0.0.1:8091");
        c.web_host = "10.1.2.3".into();
        assert_eq!(health_address(&c).to_string(), "10.1.2.3:8091");
        c.web_host = "localhost".into();
        assert_eq!(health_address(&c).to_string(), "127.0.0.1:8091");
        assert!(is_loopback("127.0.0.1") && !is_loopback("0.0.0.0"));
    }

    #[test]
    fn tools_are_replaced_when_different_and_self_updating_ones_only_when_missing() {
        let d = tempfile::tempdir().unwrap();
        let (from, to) = (d.path().join("from"), d.path().join("to"));
        std::fs::create_dir_all(from.join("yt-dlp")).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("ffmpeg.exe"), b"new ffmpeg").unwrap();
        std::fs::write(from.join("avcodec.dll"), b"dll").unwrap();
        std::fs::write(from.join("deno.exe"), b"release deno").unwrap();
        std::fs::write(from.join("yt-dlp").join("yt-dlp.exe"), b"release yt-dlp").unwrap();
        std::fs::write(from.join("notes.txt"), b"not a tool").unwrap();
        std::fs::write(to.join("ffmpeg.exe"), b"old ffmpeg").unwrap();
        std::fs::write(to.join("deno.exe"), b"nightly-updated deno").unwrap();

        copy_tools(&from, &to).unwrap();
        assert_eq!(std::fs::read(to.join("ffmpeg.exe")).unwrap(), b"new ffmpeg");
        assert_eq!(std::fs::read(to.join("avcodec.dll")).unwrap(), b"dll");
        assert_eq!(std::fs::read(to.join("deno.exe")).unwrap(), b"nightly-updated deno", "never downgraded");
        assert_eq!(std::fs::read(to.join("yt-dlp").join("yt-dlp.exe")).unwrap(), b"release yt-dlp", "installed when missing");
        assert!(!to.join("notes.txt").exists());
    }

    #[test]
    fn an_import_keeps_the_watchfolder_where_it_was_and_moves_the_state() {
        // Moving this PC's production from D:\OmniDownloader: a relative
        // "watchfolder" copied as-is would quietly point into the new folder.
        let d = tempfile::tempdir().unwrap();
        let (old, new_root) = (d.path().join("old"), d.path().join("new"));
        std::fs::create_dir_all(old.join("data").join("adblock")).unwrap();
        let mut c = AppConfig::default();
        c.watchfolder_path = "watchfolder".into();
        c.web_port = 8080;
        c.save_to_file(old.join("config.json")).unwrap();
        std::fs::write(old.join("data").join("omni.db"), b"db").unwrap();
        std::fs::write(old.join("data").join("secrets.bin"), b"dpapi").unwrap();
        std::fs::write(old.join("data").join("adblock").join("list.txt"), b"x").unwrap();

        let new = AppPaths::with_root(&new_root, "config.json");
        let imported = import_from(&old, &new).unwrap();
        assert_eq!(PathBuf::from(&imported.watchfolder_path), AppPaths::with_root(&old, "config.json").resolve("watchfolder"));
        assert_eq!(imported.database_path, "data/omni.db");
        assert_eq!(std::fs::read(new.data.join("secrets.bin")).unwrap(), b"dpapi");
        assert!(new.data.join("adblock").join("list.txt").exists());

        // Never over an installation that has state of its own.
        imported.save_to_file(&new.config).unwrap();
        assert!(import_from(&old, &new).is_err());

        // Nor from a database the old daemon still writes to.
        let busy = AppPaths::with_root(d.path().join("other"), "config.json");
        std::fs::write(old.join("data").join("omni.db-wal"), b"pending pages").unwrap();
        let why = import_from(&old, &busy).unwrap_err().to_string();
        assert!(why.contains("still in use"), "{why}");
    }

    #[test]
    fn the_previous_executable_is_kept_for_rollback() {
        let d = tempfile::tempdir().unwrap();
        let (src, dst) = (d.path().join("new.exe"), d.path().join("omni-ingest.exe"));
        std::fs::write(&src, b"v2").unwrap();
        assert!(!copy_executable(&src, &dst).unwrap(), "nothing to keep on a first install");
        std::fs::write(&src, b"v3").unwrap();
        assert!(copy_executable(&src, &dst).unwrap());
        assert_eq!(std::fs::read(&dst).unwrap(), b"v3");
        assert_eq!(std::fs::read(dst.with_extension("exe.prev")).unwrap(), b"v2");
    }
}
