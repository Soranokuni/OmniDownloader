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
    can_install_services, install_service, installed_service_exe, needs_network_identity, start_service,
    stop_service_and_wait, uninstall_service, ServiceAccount,
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

    // The service is (re)registered when it is missing, points elsewhere, or
    // a new account was asked for. Otherwise its account is kept as it is.
    let register = service_exe.as_deref().map_or(true, |p| !same_file(p, &target_exe)) || o.account.is_some();
    let account = if register {
        choose_account(o.account.as_deref(), &config.watchfolder_path, o.yes)?
    } else {
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
            account.as_ref().map(|a| a.name.as_str()).unwrap_or("LocalSystem")
        );
        if let Some(a) = &account {
            grant_modify(&a.name, &paths.root).await;
            if !needs_network_identity(&config.watchfolder_path) {
                grant_modify(&a.name, &watch).await;
            }
        }
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
        println!("  Next, on THIS computer:");
        println!("   1. Open {scheme}://127.0.0.1:{}/setup and create the first administrator.", config.web_port);
        println!("   2. In Admin, set the mailbox (Microsoft Graph) and, if wanted, the LLM key.");
        println!("   3. In Admin → Security, add the newsroom network so the MCR desks can open the panel.");
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

fn choose_account(account: Option<&str>, watchfolder: &str, yes: bool) -> Result<Option<ServiceAccount>> {
    let Some(name) = account.map(str::trim).filter(|s| !s.is_empty()) else {
        if needs_network_identity(watchfolder) {
            println!();
            println!("! The watchfolder {watchfolder} is a network share. As LocalSystem the service");
            println!("  reaches it as this computer's account, which most file servers refuse; files");
            println!("  then fail at the very last step. Install with an account that has Modify on");
            println!("  the share:   Install.cmd --account \"DOMAIN\\svc_omni\"");
            if yes {
                bail!("A network watchfolder needs --account.");
            }
            let go = inquire::Confirm::new("Install as LocalSystem anyway?").with_default(false).prompt()?;
            if !go {
                bail!("Installation cancelled.");
            }
        }
        return Ok(None);
    };
    if ServiceAccount::is_passwordless_builtin(name) {
        return Ok(Some(ServiceAccount { name: name.to_string(), password: None }));
    }
    let password = inquire::Password::new(&format!("Password for {name}:"))
        .without_confirmation()
        .prompt()
        .context("No password given")?;
    Ok(Some(ServiceAccount::with_password(name, password)))
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
async fn grant_modify(account: &str, dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let grant = format!("{account}:(OI)(CI)M");
    let args = [dir.as_os_str().to_os_string(), "/grant".into(), grant.into(), "/T".into(), "/Q".into()];
    match omni_core::process::run_capture(Path::new("icacls"), args, Duration::from_secs(120)).await {
        Ok(o) if o.success => println!("  {account} may modify {}.", dir.display()),
        _ => println!("! Could not give {account} Modify on {}; set it by hand.", dir.display()),
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
