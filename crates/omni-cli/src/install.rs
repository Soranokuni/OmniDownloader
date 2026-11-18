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
//!
//! The work is split in two so the console and a graphical installer share it
//! (plan P13.1a): [`plan_install`] decides everything without changing the
//! machine and lists the questions a front end still has to ask; [`execute`]
//! performs the machine steps of a plan and reports each line through a
//! [`Progress`]. [`install`] is the console front end: plan, ask with
//! `inquire`, execute.

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use inquire::Text;

use omni_core::config::AppConfig;
use omni_core::paths::{AppPaths, DriveLookup};
use omni_service::{
    can_install_services, default_or_given, install_service, installed_service_account, installed_service_exe,
    needs_network_identity_with, network_share_warning_applies, start_service,
    stop_service_and_wait, uninstall_service, AccountChoice, ServiceAccount, DEFAULT_SERVICE_ACCOUNT,
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
    /// The starting configuration of a new installation (an answer file, P13.1b),
    /// used instead of the defaults only when there is no config.json and no import.
    /// Holds no secret: the answer file cannot carry one.
    pub base_config: Option<AppConfig>,
}

impl InstallOptions {
    /// Start a new installation from `config`. Used only when there is no
    /// config.json and no import; its watchfolder is then judged like
    /// `--watchfolder`'s, so the same refusals apply to it.
    pub fn with_base_config(mut self, config: AppConfig) -> Self {
        self.base_config = Some(config);
        self
    }
}

// ---- what the plan looks at --------------------------------------------------

/// Everything [`plan_install`] reads from the machine. [`SystemProbe`] asks
/// Windows and the file system; tests use a fake. Read-only by contract.
pub trait Probe {
    /// Whether this process may register services (an elevated administrator).
    fn can_install_services(&self) -> bool;
    /// The executable being installed; the release folder is its directory.
    fn current_exe(&self) -> std::io::Result<PathBuf>;
    /// The registered service's executable, when the service is installed.
    fn installed_exe(&self) -> Option<PathBuf>;
    /// The account the registered service runs as.
    fn installed_account(&self) -> Option<String>;
    fn exists(&self, path: &Path) -> bool;
    fn is_file(&self, path: &Path) -> bool;
    /// Size in bytes; `None` when it cannot be read.
    fn file_len(&self, path: &Path) -> Option<u64>;
    /// An existing config.json.
    fn load_config(&self, path: &Path) -> Result<AppConfig>;
    /// Whether two paths name the same file or folder (canonical, case-folded).
    fn same_dir(&self, a: &Path, b: &Path) -> bool;
    /// Drive letters, for the mapped-drive refusal (`watchfolder_problem_with`)
    /// and the network-share test (`needs_network_identity_with`).
    fn drives(&self) -> &dyn DriveLookup;
    /// For an existing folder, its owner when that is none of Administrators,
    /// SYSTEM, TrustedInstaller or this process's user; `None` otherwise.
    fn foreign_owner(&self, dir: &Path) -> Option<String>;
    /// Whether Windows knows the account `name`.
    fn account_exists(&self, name: &str) -> bool;
    /// Whether `path` is a link (junction, symbolic link, mount point); not
    /// any other reparse point (`fsutil::is_link`).
    fn is_link(&self, path: &Path) -> bool;
    /// The first existing folder strictly between the volume root and `root`
    /// that someone besides Administrators, SYSTEM and TrustedInstaller may
    /// rename or re-permission (or that such an account owns), with its name.
    fn swappable_ancestor(&self, root: &Path) -> Option<(PathBuf, String)>;
    /// The first link at or anywhere below `dir`, never entering one; `None`
    /// for a missing `dir`.
    fn first_link_under(&self, dir: &Path) -> Option<PathBuf>;
    /// Whether a file's owner is Administrators, SYSTEM or TrustedInstaller.
    fn trusted_owner(&self, path: &Path) -> bool;
    /// Who besides those (and CREATOR OWNER) may put files into `dir` or take
    /// it over, inherit-only entries included.
    fn writable_by(&self, dir: &Path) -> Option<String>;
}

/// The real machine.
pub struct SystemProbe;

impl Probe for SystemProbe {
    fn can_install_services(&self) -> bool {
        can_install_services()
    }
    fn current_exe(&self) -> std::io::Result<PathBuf> {
        std::env::current_exe()
    }
    fn installed_exe(&self) -> Option<PathBuf> {
        installed_service_exe()
    }
    fn installed_account(&self) -> Option<String> {
        installed_service_account()
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn file_len(&self, path: &Path) -> Option<u64> {
        std::fs::metadata(path).ok().map(|m| m.len())
    }
    fn load_config(&self, path: &Path) -> Result<AppConfig> {
        AppConfig::load_from_file(path)
    }
    fn same_dir(&self, a: &Path, b: &Path) -> bool {
        same_dir(a, b)
    }
    fn drives(&self) -> &dyn DriveLookup {
        &omni_core::paths::SystemDrives
    }
    fn foreign_owner(&self, dir: &Path) -> Option<String> {
        omni_core::lockdown::foreign_owner(dir)
    }
    fn account_exists(&self, name: &str) -> bool {
        omni_core::lockdown::account_exists(name)
    }
    fn is_link(&self, path: &Path) -> bool {
        omni_core::fsutil::is_link(path)
    }
    fn swappable_ancestor(&self, root: &Path) -> Option<(PathBuf, String)> {
        // Strictly between the volume root (no parent) and `root`: owner and entries.
        root.ancestors()
            .skip(1)
            .filter(|a| a.parent().is_some() && a.is_dir())
            .find_map(|a| omni_core::lockdown::folder_swapper(a).map(|who| (a.to_path_buf(), who)))
    }
    fn first_link_under(&self, dir: &Path) -> Option<PathBuf> {
        omni_core::fsutil::first_link_in_tree(dir, &omni_core::fsutil::is_link)
    }
    fn trusted_owner(&self, path: &Path) -> bool {
        omni_core::lockdown::owner_sid(path).is_some_and(|sid| omni_core::lockdown::is_trusted_owner(&sid))
    }
    fn writable_by(&self, dir: &Path) -> Option<String> {
        omni_core::lockdown::writable_by(dir)
    }
}

// ---- the plan ----------------------------------------------------------------

/// New installation or upgrade, decided before anything changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    Fresh,
    /// The executable is in the install folder already, or the service is registered.
    Upgrade { had_previous_exe: bool },
}

/// Where the watchfolder written to config.json comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchfolderSource {
    /// `--watchfolder`.
    Options,
    /// The imported installation's, made absolute.
    Import,
    /// The existing config.json's, left as it is.
    ExistingConfig,
    /// `<install folder>\watchfolder`.
    Default,
    /// What the operator answered to [`Question::NeedWatchfolder`].
    Answered,
}

/// What the service registration runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountDecision {
    /// Not registered again: the installed service keeps the account it has.
    Keep { current: Option<String> },
    /// (Re)registered as LocalSystem (`--account LocalSystem`).
    LocalSystem,
    /// (Re)registered as an account Windows takes no password for (the
    /// default virtual account `NT SERVICE\OmniIngestService`).
    Passwordless(String),
    /// (Re)registered as a domain or local user account; the password comes
    /// in [`Answers::password`].
    NeedsPassword(String),
}

impl AccountDecision {
    /// Whether the service is (re)registered (today's `account_plan().register`).
    pub fn registers(&self) -> bool {
        !matches!(self, AccountDecision::Keep { .. })
    }
}

/// What a front end still has to ask before [`execute`], in this order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// A new configuration and no `--watchfolder`/`--yes`. The rest of the plan
    /// assumes `default` until the answer is given to [`plan_answered`].
    NeedWatchfolder { default: String },
    /// A network watchfolder with the defaulted account: show `text`, then ask
    /// `prompt` (default No). No means "Installation cancelled.".
    ConfirmUncWithoutAccount { text: Vec<String>, prompt: String },
    /// The password of a named account.
    NeedPassword { account: String },
}

/// Where the install goes and what is there already: the first part of a plan,
/// known before any configuration is read (the console prints it first).
#[derive(Debug, Clone)]
pub struct Target {
    /// `--dir` as given, or the default; created first.
    pub dir: PathBuf,
    pub paths: AppPaths,
    /// The executable being installed and the release folder it is in.
    pub source_exe: PathBuf,
    pub source_dir: PathBuf,
    pub target_exe: PathBuf,
    /// Run from the install folder itself: nothing is copied.
    pub in_place: bool,
    /// The registered service's executable: it is stopped first, and removed
    /// before a new registration.
    pub service_exe: Option<PathBuf>,
    pub kind: InstallKind,
    /// What an upgrade carries on despite (a folder above the root others may
    /// swap); printed after the header.
    pub warnings: Vec<String>,
    /// A cargo build run in place (see [`is_cargo_build`]): never locked or
    /// judged, created plainly. Decided by the executable, never by the root's
    /// surroundings (a `Cargo.toml` anyone may write).
    pub development: bool,
}

impl Target {
    /// What the console prints before anything else.
    pub fn header(&self) -> Vec<String> {
        let mut lines = vec![
            String::new(),
            format!("OmniDownloader {} → {}", env!("CARGO_PKG_VERSION"), self.paths.root.display()),
            format!(
                "  {}",
                if self.is_upgrade() { "Upgrading the existing installation." } else { "New installation." }
            ),
        ];
        lines.extend(self.warnings.iter().cloned());
        lines
    }

    pub fn is_upgrade(&self) -> bool {
        matches!(self.kind, InstallKind::Upgrade { .. })
    }

    /// The install folder already holds an installation (its `omni-ingest.exe`).
    pub fn installed_here(&self) -> bool {
        matches!(self.kind, InstallKind::Upgrade { had_previous_exe: true })
    }
}

/// Every decision of an install, made without changing the machine.
#[derive(Clone)]
pub struct InstallPlan {
    pub target: Target,
    /// `--import`: the old installation whose state is copied in first.
    pub import: Option<PathBuf>,
    /// The configuration the service will run with (imported, existing or
    /// default, with the command line applied). Not in `Debug`: an old
    /// config.json may still carry a legacy secret.
    pub config: AppConfig,
    /// config.json is written (new, or changed by `--watchfolder`/`--port`).
    pub write_config: bool,
    pub watchfolder_source: WatchfolderSource,
    /// The watchfolder resolved against the install folder.
    pub watchfolder: PathBuf,
    /// A local watchfolder is created; a network one is not ours to create.
    pub create_watchfolder: bool,
    pub port: u16,
    pub account: AccountDecision,
    /// A kept LocalSystem service: tell the operator how to switch.
    pub switch_hint: bool,
    /// The registered service's account, whose old grants are stripped.
    pub previous_account: Option<String>,
    /// The ACL steps when the service is registered (`grant_plan`); empty otherwise.
    pub grants: Vec<(PathBuf, Rights)>,
    /// The firewall rule's port, unless `--no-firewall` or a loopback panel.
    pub firewall: Option<u16>,
    /// Where `/api/health` is asked, and whether the panel speaks TLS.
    pub health: SocketAddr,
    pub tls: bool,
    pub questions: Vec<Question>,
}

impl fmt::Debug for InstallPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallPlan")
            .field("target", &self.target)
            .field("import", &self.import)
            .field("watchfolder_path", &self.config.watchfolder_path)
            .field("web_host", &self.config.web_host)
            .field("write_config", &self.write_config)
            .field("watchfolder_source", &self.watchfolder_source)
            .field("watchfolder", &self.watchfolder)
            .field("create_watchfolder", &self.create_watchfolder)
            .field("port", &self.port)
            .field("account", &self.account)
            .field("switch_hint", &self.switch_hint)
            .field("previous_account", &self.previous_account)
            .field("grants", &self.grants)
            .field("firewall", &self.firewall)
            .field("health", &self.health)
            .field("tls", &self.tls)
            .field("questions", &self.questions)
            .finish()
    }
}

impl InstallPlan {
    /// The default of an open [`Question::NeedWatchfolder`].
    pub fn needs_watchfolder(&self) -> Option<&str> {
        self.questions.iter().find_map(|q| match q {
            Question::NeedWatchfolder { default } => Some(default.as_str()),
            _ => None,
        })
    }

    /// The account to register with, once every question is answered.
    fn service_account(&self, answers: &Answers) -> Result<Option<ServiceAccount>> {
        for q in &self.questions {
            match q {
                Question::NeedWatchfolder { .. } => {
                    bail!("The watchfolder was not given; plan again with the answer.")
                }
                Question::ConfirmUncWithoutAccount { .. } if !answers.confirm_unc => return Err(Cancelled.into()),
                _ => {}
            }
        }
        Ok(match &self.account {
            AccountDecision::Keep { .. } | AccountDecision::LocalSystem => None,
            AccountDecision::Passwordless(name) => Some(ServiceAccount { name: name.clone(), password: None }),
            AccountDecision::NeedsPassword(name) => {
                let password = answers.password.clone().context("No password given")?;
                Some(ServiceAccount::with_password(name.clone(), password))
            }
        })
    }
}

/// What the front end collected. `watchfolder` goes to [`plan_answered`];
/// the rest to [`execute`]. The password is never printed (`Debug` hides it).
#[derive(Clone, Default)]
pub struct Answers {
    pub watchfolder: Option<String>,
    /// Yes to [`Question::ConfirmUncWithoutAccount`].
    pub confirm_unc: bool,
    pub password: Option<String>,
}

impl fmt::Debug for Answers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Answers")
            .field("watchfolder", &self.watchfolder)
            .field("confirm_unc", &self.confirm_unc)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .finish()
    }
}

/// A refusal of the plan with lines to show before the error (the network-share
/// warning under `--yes`). Its `Display` is the error the console always printed.
#[derive(Debug)]
pub struct Refusal {
    pub notice: Vec<String>,
    pub message: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

/// The operator said no, or aborted a prompt (Esc, Ctrl-C): nothing was changed.
/// Its `Display` is the line the console always printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Installation cancelled.")
    }
}

impl std::error::Error for Cancelled {}

/// A prompt's answer; Esc or Ctrl-C is [`Cancelled`], any other failure is `what`.
fn answered<T>(r: std::result::Result<T, inquire::InquireError>, what: &'static str) -> Result<T> {
    use inquire::InquireError::{OperationCanceled, OperationInterrupted};
    match r {
        Ok(v) => Ok(v),
        Err(OperationCanceled | OperationInterrupted) => Err(Cancelled.into()),
        Err(e) => Err(anyhow::Error::new(e).context(what)),
    }
}

const SHARE_REFUSAL: &str =
    "A network watchfolder needs --account (a domain account), or the computer account granted on the share.";

fn foreign_owner_refusal(dir: &Path, owner: &str) -> String {
    format!(
        "{} exists and belongs to {owner}, not to Administrators. Setup does not install into a folder another \
         account controls. Remove the folder or choose another one; if it is yours, run: icacls \"{}\" /setowner Administrators /T",
        dir.display(),
        dir.display()
    )
}

fn unknown_account_refusal(name: &str) -> String {
    format!("Windows does not know the account {name}; check the spelling (DOMAIN\\user or .\\user).")
}

/// Where the install goes: the administrator check, the release folder, the
/// install folder and whether something is installed there already.
pub fn locate(o: &InstallOptions, probe: &dyn Probe) -> Result<Target> {
    if !probe.can_install_services() {
        bail!(
            "The installer needs administrator rights. Run Install.cmd (it asks for them), \
             or run this from an elevated terminal."
        );
    }
    let source_exe = probe.current_exe().context("Cannot find this executable")?;
    let source_dir = source_exe.parent().context("No release folder")?.to_path_buf();
    let dir = o.dir.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    let paths = AppPaths::with_root(&dir, "config.json");
    // Every later write and grant would follow a link (P13.1b, S2).
    if let Some(link) = paths.root.ancestors().find(|a| probe.is_link(a)) {
        return Err(Refusal { notice: Vec::new(), message: link_refusal(&paths.root, link) }.into());
    }
    let target_exe = paths.root.join(EXE);
    let service_exe = probe.installed_exe();
    let in_place = probe.same_dir(&source_dir, &paths.root);
    // A cargo build run in place is never locked, so it is not judged either (T1).
    let development = in_place && is_cargo_build(&source_exe, &|p| probe.is_file(p));
    // An installation is here only when its executable is one an administrator
    // put there: a planted one makes nothing an upgrade (U3).
    let had_previous_exe = probe.exists(&target_exe) && (development || probe.trusted_owner(&target_exe));
    let kind = if had_previous_exe || service_exe.is_some() {
        InstallKind::Upgrade { had_previous_exe }
    } else {
        InstallKind::Fresh
    };
    let mut warnings = Vec::new();
    if !development {
        // Whoever owns the folder can always rewrite its rights, whatever the
        // lockdown sets, and replace what is in it (P13.1b, R2/S4).
        if let Some(owner) = probe.foreign_owner(&paths.root) {
            return Err(Refusal { notice: Vec::new(), message: foreign_owner_refusal(&paths.root, &owner) }.into());
        }
        // An existing folder that holds no installation keeps whatever rights it
        // was made with until the lockdown: nobody else may be able to put a file
        // in it meanwhile (U3). A locked leftover of an uninstall passes.
        if !had_previous_exe && probe.exists(&paths.root) {
            if let Some(who) = probe.writable_by(&paths.root) {
                return Err(Refusal { notice: Vec::new(), message: writable_refusal(&paths.root, &who) }.into());
            }
        }
        // A folder above the root that someone else may rename or owns lets
        // them swap the whole installation (S2c, T6). An installation already
        // in this folder is never blocked for a condition it already had: it is
        // told (T5).
        if let Some((ancestor, who)) = probe.swappable_ancestor(&paths.root) {
            let text = swappable_refusal(&ancestor, &who);
            if !had_previous_exe {
                return Err(Refusal { notice: Vec::new(), message: text }.into());
            }
            warnings.push(format!("! {text}"));
        }
    }
    Ok(Target { dir, paths, source_exe, source_dir, target_exe, in_place, service_exe, kind, warnings, development })
}

/// Whether `exe` is cargo's output: some folder above it is named `target` and
/// that folder's parent holds a `Cargo.toml` (`<repo>\target\…\omni-ingest.exe`).
/// Judged on the executable only, never on the install root's surroundings,
/// where anyone may put a `Cargo.toml` (T1). Pure but for `is_file`.
pub fn is_cargo_build(exe: &Path, is_file: &dyn Fn(&Path) -> bool) -> bool {
    exe.ancestors().skip(1).any(|a| {
        a.file_name().is_some_and(|n| n.eq_ignore_ascii_case("target"))
            && a.parent().is_some_and(|repo| is_file(&repo.join("Cargo.toml")))
    })
}

fn writable_refusal(dir: &Path, who: &str) -> String {
    format!(
        "{} already exists and {who} can write to it, so files could be planted in it before Setup secures it. Remove \
         the folder and let Setup create it, or choose another folder. Nothing was changed.",
        dir.display()
    )
}

fn link_inside_refusal(link: &Path) -> String {
    format!(
        "{} is a link inside the installation. Setup does not copy or change permissions through a link. Remove it, \
         or give the real folder in config.json, and run Setup again. Nothing was changed.",
        link.display()
    )
}

/// The first link this run would write or change permissions through (T3/T7):
/// always, the state folders it creates or copies into (`bin`, `data`, `logs`,
/// `temp`, `archive` and what `ensure_dirs` makes in them) and the way down to
/// them; when it changes permissions (`registers`), also anything in the
/// root's tree and in the tree of every path it grants on (a local watchfolder
/// outside the root included; a share is not walked). A cargo build in place
/// is never locked, so its (large) root tree is not walked.
fn planned_link(t: &Target, grants: &[(PathBuf, Rights)], registers: bool, probe: &dyn Probe) -> Option<PathBuf> {
    let p = &t.paths;
    let state = [&p.bin, &p.data, &p.logs, &p.temp, &p.archive, &p.temp.join("jobs"), &p.data.join("adblock")];
    let is_link = |x: &Path| probe.is_link(x);
    for d in state {
        if let Some(link) = omni_core::fsutil::link_below(&p.root, d, &is_link) {
            return Some(link);
        }
    }
    if !registers {
        return None;
    }
    if !t.development {
        if let Some(link) = probe.first_link_under(&p.root) {
            return Some(link);
        }
    }
    // What lies inside the root was walked with it (or, for a cargo build in
    // place, is never locked): only the paths outside it, each once (U4).
    let mut outside: Vec<&PathBuf> = Vec::new();
    for (path, _) in grants {
        let s = path.to_string_lossy();
        let share = s.starts_with(r"\\") || s.starts_with("//");
        if !share && !path.starts_with(&p.root) && !outside.contains(&path) {
            outside.push(path);
        }
    }
    outside.into_iter().find_map(|path| probe.first_link_under(path))
}

fn link_refusal(dir: &Path, link: &Path) -> String {
    format!("{} is reached through a link ({}). Give the real folder with /dir.", dir.display(), link.display())
}

fn swappable_refusal(ancestor: &Path, who: &str) -> String {
    format!(
        "{} can be renamed or re-permissioned by {who}, so the service's folder could be swapped. Install directly on a \
         drive (for example D:\\OmniIngest) or under Program Files, or remove that permission.",
        ancestor.display()
    )
}

/// Decide the whole install without changing the machine. A fresh
/// configuration without `--watchfolder` or `--yes` comes back with
/// [`Question::NeedWatchfolder`]: ask it, then call [`plan_answered`].
pub fn plan_install(o: &InstallOptions, probe: &dyn Probe) -> Result<InstallPlan> {
    plan_answered(o, &Answers::default(), probe)
}

/// [`plan_install`] with the watchfolder the operator gave.
pub fn plan_answered(o: &InstallOptions, answers: &Answers, probe: &dyn Probe) -> Result<InstallPlan> {
    plan_from(locate(o, probe)?, o, answers, probe)
}

fn plan_from(target: Target, o: &InstallOptions, answers: &Answers, probe: &dyn Probe) -> Result<InstallPlan> {
    let paths = &target.paths;
    let mut questions = Vec::new();

    // ---- configuration ------------------------------------------------------
    let imported = match &o.import {
        Some(old) => Some(imported_config(old, paths, probe)?.0),
        None => None,
    };
    let fresh_config = !probe.exists(&paths.config);
    let mut config = if let Some(c) = imported.clone() {
        c
    } else if fresh_config {
        o.base_config.clone().unwrap_or_default()
    } else {
        probe.load_config(&paths.config).context("The existing config.json cannot be read")?
    };
    let default_watch = paths.root.join("watchfolder").display().to_string();
    // An answer file's watchfolder counts only when its configuration is the one used.
    let base_used = imported.is_none() && fresh_config && o.base_config.is_some();
    let given_watch = o.watchfolder.clone().or_else(|| base_used.then(|| config.watchfolder_path.clone()));
    let watchfolder_source = if let Some(w) = &given_watch {
        config.watchfolder_path = w.clone();
        WatchfolderSource::Options
    } else if imported.is_some() {
        // The old installation's watchfolder, made absolute by the import.
        WatchfolderSource::Import
    } else if fresh_config && !o.yes {
        match &answers.watchfolder {
            Some(answer) => {
                config.watchfolder_path = answer.trim().to_string();
                WatchfolderSource::Answered
            }
            None => {
                questions.push(Question::NeedWatchfolder { default: default_watch.clone() });
                config.watchfolder_path = default_watch;
                WatchfolderSource::Default
            }
        }
    } else if fresh_config {
        config.watchfolder_path = default_watch;
        WatchfolderSource::Default
    } else {
        WatchfolderSource::ExistingConfig
    };
    if let Some(p) = o.port {
        config.web_port = p;
    }
    // Before anything is stopped or copied. A relative value in config.json is
    // legitimate (it means "under the install folder"), so judge the resolved path.
    // With `--dir` on a mapped drive the default is refused before it is asked (P13.1a; accepted).
    check_config_watchfolder(paths, &config, probe.drives())?;

    // ---- service account ------------------------------------------------------
    // The service is (re)registered when it is missing, points elsewhere, or
    // a new account was asked for. Otherwise its account is kept as it is.
    let previous_account = probe.installed_account();
    let same = |a: &Path, b: &Path| probe.same_dir(a, b);
    let decision = account_plan_with(
        target.service_exe.as_deref(),
        &target.target_exe,
        o.account.as_deref(),
        previous_account.as_deref(),
        &same,
    );
    let network_watch = needs_network_identity_with(&config.watchfolder_path, probe.drives());
    let account = if decision.register {
        // The decision `choose_account` makes, with its prompts turned into questions.
        let choice = default_or_given(o.account.as_deref());
        let defaulted = o.account.as_deref().map_or(true, |s| s.trim().is_empty());
        // `network_share_warning_applies`, judged with the probe's drives.
        if defaulted && network_watch && !matches!(choice, AccountChoice::NeedsPassword(_)) {
            let who = choice_name(&choice);
            let text = share_warning(&config.watchfolder_path, who);
            if o.yes {
                return Err(Refusal { notice: text, message: SHARE_REFUSAL.into() }.into());
            }
            questions.push(Question::ConfirmUncWithoutAccount { text, prompt: format!("Install as {who} anyway?") });
        }
        match choice {
            AccountChoice::LocalSystem => AccountDecision::LocalSystem,
            AccountChoice::Passwordless(name) => AccountDecision::Passwordless(name),
            AccountChoice::NeedsPassword(name) => {
                // A misspelt account would leave the machine without a service:
                // the old registration is removed before the new one (P13.1b, R5).
                if !probe.account_exists(&name) {
                    return Err(Refusal { notice: Vec::new(), message: unknown_account_refusal(&name) }.into());
                }
                questions.push(Question::NeedPassword { account: name.clone() });
                AccountDecision::NeedsPassword(name)
            }
        }
    } else {
        AccountDecision::Keep { current: previous_account.clone() }
    };
    let grants = if account.registers() {
        let named = !matches!(account, AccountDecision::LocalSystem);
        grant_plan_with(paths, &config, named, probe.drives())
    } else {
        Vec::new()
    };
    // Before the service is stopped: what this run writes into or changes the
    // permissions of must hold no link (T3/T7). The execute-time guards stay
    // as the backstop for a link made in between.
    if let Some(link) = planned_link(&target, &grants, account.registers(), probe) {
        return Err(Refusal { notice: Vec::new(), message: link_inside_refusal(&link) }.into());
    }

    Ok(InstallPlan {
        import: o.import.clone(),
        write_config: fresh_config || o.watchfolder.is_some() || o.port.is_some(),
        watchfolder_source,
        watchfolder: paths.resolve(&config.watchfolder_path),
        create_watchfolder: !network_watch,
        port: config.web_port,
        account,
        switch_hint: decision.switch_hint,
        previous_account,
        grants,
        firewall: (!o.no_firewall && !is_loopback(&config.web_host)).then_some(config.web_port),
        health: health_address(&config),
        tls: config.tls.is_enabled(),
        questions,
        config,
        target,
    })
}

/// The import's checks and the configuration it brings, without copying:
/// the old config with its watchfolder made absolute and the database moved to
/// `data/omni.db`, plus the old database file.
fn imported_config(old: &Path, new: &AppPaths, probe: &dyn Probe) -> Result<(AppConfig, PathBuf, AppPaths)> {
    let old_paths = AppPaths::with_root(old, "config.json");
    if !probe.is_file(&old_paths.config) {
        bail!("{} has no config.json to import", old.display());
    }
    if probe.exists(&new.config) || probe.exists(&new.data.join("omni.db")) {
        bail!("{} already has a configuration or a database; import only into a new installation", new.root.display());
    }
    let mut config = probe.load_config(&old_paths.config).context("The old config.json cannot be read")?;
    let db = old_paths.resolve(&config.database_path);
    let wal = db.with_extension("db-wal");
    if probe.file_len(&wal).map_or(false, |n| n > 0) {
        bail!(
            "{} is still in use (its write-ahead log is not empty). Stop the old OmniDownloader first \
             (close its console window or stop its service), then run the installer again.",
            db.display()
        );
    }
    config.watchfolder_path = absolute(&old_paths, &config.watchfolder_path);
    config.database_path = "data/omni.db".into();
    Ok((config, db, old_paths))
}

/// Bring an older installation's state into a new, empty one: its
/// config.json, data\omni.db, data\secrets.bin (machine-scope DPAPI: valid on
/// this PC only), seeds and adblock lists. Paths in its config that were
/// relative to the old folder and point at things that stay there (the
/// watchfolder) are made absolute, so they still point where they did; the
/// database, temp and archive paths stay relative and follow the move.
fn import_from(old: &Path, new: &AppPaths, progress: &mut dyn Progress) -> Result<AppConfig> {
    let (config, db, old_paths) = imported_config(old, new, &SystemProbe)?;
    std::fs::create_dir_all(&new.data)?;
    std::fs::copy(&db, new.data.join("omni.db")).with_context(|| format!("Cannot copy {}", db.display()))?;
    for f in ["secrets.bin", "journalists.seed.json", "taxonomy.json"] {
        let src = old_paths.data.join(f);
        if src.is_file() {
            std::fs::copy(&src, new.data.join(f))?;
        }
    }
    if old_paths.data.join("adblock").is_dir() {
        copy_recursive(&old_paths.data.join("adblock"), &new.data.join("adblock"))?;
    }
    progress.line(Step::Import, &format!("  Imported the configuration, database, secrets and seeds from {}.", old.display()));
    progress.line(Step::Import, &format!("  Watchfolder stays {}.", config.watchfolder_path));
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
        // A refusal, so every front end reports "nothing was changed" (exit 5 in Setup).
        Some(problem) => Err(Refusal { notice: Vec::new(), message: format!("Cannot install: {problem}") }.into()),
        None => Ok(()),
    }
}

/// A config path as an absolute one, resolved against the folder it was
/// relative to; UNC and absolute paths unchanged.
fn absolute(paths: &AppPaths, p: &str) -> String {
    paths.resolve(p).display().to_string()
}

// ---- execution -----------------------------------------------------------------

/// Which part of the install a progress line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Plan,
    Import,
    Account,
    Stop,
    Copy,
    Config,
    Register,
    Rights,
    Firewall,
    Start,
    Health,
    Rollback,
    Done,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Plan => "plan",
            Step::Import => "import",
            Step::Account => "account",
            Step::Stop => "stop",
            Step::Copy => "copy",
            Step::Config => "config",
            Step::Register => "register",
            Step::Rights => "rights",
            Step::Firewall => "firewall",
            Step::Start => "start",
            Step::Health => "health",
            Step::Rollback => "rollback",
            Step::Done => "done",
        }
    }
}

/// Receives every line an install has to say, as it happens. Lines keep their
/// console indentation; `""` is a blank line.
pub trait Progress {
    fn line(&mut self, step: Step, text: &str);
}

/// The console: each line printed as it comes.
pub struct ConsoleProgress;

impl Progress for ConsoleProgress {
    fn line(&mut self, _step: Step, text: &str) {
        println!("{text}");
    }
}

/// What an install did. Never holds the password.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub kind: InstallKind,
    /// The new version did not answer and the previous one was put back and started.
    pub rolled_back: bool,
    /// Where the operator goes next, on this computer.
    pub admin_url: String,
    /// The health summary lines the service reported (empty under TLS).
    pub health: Vec<String>,
    /// The log folder, for a failure.
    pub logs: PathBuf,
}

/// The machine steps of an install, one method per step, so the order of
/// [`execute`] is tested without touching the machine.
pub(crate) trait Machine {
    /// The install folder, checked again first (T2).
    fn create_dir(&mut self, t: &Target) -> Result<()>;
    fn import(&mut self, old: &Path, new: &AppPaths, progress: &mut dyn Progress) -> Result<()>;
    fn stop_service(&mut self) -> Result<()>;
    fn copy_executable(&mut self, source: &Path, target: &Path, progress: &mut dyn Progress) -> Result<bool>;
    fn copy_release(&mut self, source_dir: &Path, paths: &AppPaths, progress: &mut dyn Progress) -> Result<()>;
    fn ensure_dirs(&mut self, paths: &AppPaths) -> Result<()>;
    fn create_watchfolder(&mut self, dir: &Path);
    fn save_config(&mut self, config: &AppConfig, path: &Path) -> Result<()>;
    fn uninstall_service(&mut self) -> Result<()>;
    fn install_service(&mut self, exe: &Path, account: Option<&ServiceAccount>) -> Result<()>;
    async fn apply_acls(
        &mut self,
        plan: &InstallPlan,
        account: Option<&ServiceAccount>,
        progress: &mut dyn Progress,
    );
    async fn open_firewall(&mut self, exe: &Path, port: u16, progress: &mut dyn Progress);
    fn start_service(&mut self) -> Result<()>;
    fn wait_for_health(&mut self, addr: SocketAddr, tls: bool, timeout: Duration) -> Option<String>;
    fn restore_previous(&mut self, target_exe: &Path) -> Result<()>;
}

struct SystemMachine;

impl Machine for SystemMachine {
    fn create_dir(&mut self, t: &Target) -> Result<()> {
        create_install_dir(t)
    }
    fn import(&mut self, old: &Path, new: &AppPaths, progress: &mut dyn Progress) -> Result<()> {
        import_from(old, new, progress).map(|_| ())
    }
    fn stop_service(&mut self) -> Result<()> {
        stop_service_and_wait(Duration::from_secs(120))
    }
    fn copy_executable(&mut self, source: &Path, target: &Path, progress: &mut dyn Progress) -> Result<bool> {
        copy_executable(source, target, progress)
    }
    fn copy_release(&mut self, source_dir: &Path, paths: &AppPaths, progress: &mut dyn Progress) -> Result<()> {
        copy_tools(&source_dir.join("bin"), &paths.bin, progress)?;
        copy_missing(&source_dir.join("data"), &paths.data, SEEDS, progress)
    }
    fn ensure_dirs(&mut self, paths: &AppPaths) -> Result<()> {
        ensure_dirs_with(paths, &omni_core::fsutil::is_link)
    }
    fn create_watchfolder(&mut self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
    }
    fn save_config(&mut self, config: &AppConfig, path: &Path) -> Result<()> {
        config.save_to_file(path)
    }
    fn uninstall_service(&mut self) -> Result<()> {
        uninstall_service()
    }
    fn install_service(&mut self, exe: &Path, account: Option<&ServiceAccount>) -> Result<()> {
        install_service(exe, account)
    }
    async fn apply_acls(&mut self, plan: &InstallPlan, account: Option<&ServiceAccount>, progress: &mut dyn Progress) {
        let t = &plan.target;
        // The installer's own executable is put directly in the root: only a cargo
        // build run in place is left unlocked (T1).
        apply_acls_on(
            &t.paths,
            &plan.config,
            account,
            plan.previous_account.as_deref(),
            !t.development,
            &plan.grants,
            progress,
            &mut SystemAcl,
        )
        .await;
    }
    async fn open_firewall(&mut self, exe: &Path, port: u16, progress: &mut dyn Progress) {
        open_firewall(exe, port, progress).await;
    }
    fn start_service(&mut self) -> Result<()> {
        start_service()
    }
    fn wait_for_health(&mut self, addr: SocketAddr, tls: bool, timeout: Duration) -> Option<String> {
        wait_for_health(addr, tls, timeout)
    }
    fn restore_previous(&mut self, target_exe: &Path) -> Result<()> {
        std::fs::copy(target_exe.with_extension("exe.prev"), target_exe)?;
        Ok(())
    }
}

/// Perform a plan: copy, configure, register, grant, open the firewall, start,
/// wait for `/api/health`, and put the previous version back when an upgrade
/// does not answer (`rolled_back`). Every question of the plan must be answered.
pub async fn execute(plan: &InstallPlan, answers: &Answers, progress: &mut dyn Progress) -> Result<InstallReport> {
    execute_with(plan, answers, progress, &mut SystemMachine).await
}

pub(crate) async fn execute_with<M: Machine>(
    plan: &InstallPlan,
    answers: &Answers,
    progress: &mut dyn Progress,
    m: &mut M,
) -> Result<InstallReport> {
    let t = &plan.target;
    let paths = &t.paths;
    // Every answer is in before the machine is touched.
    let account = plan.service_account(answers)?;
    let scheme = if plan.tls { "https" } else { "http" };
    let mut report = InstallReport {
        kind: t.kind,
        rolled_back: false,
        admin_url: format!("{scheme}://127.0.0.1:{}/admin", plan.port),
        health: Vec::new(),
        logs: paths.logs.clone(),
    };

    m.create_dir(t).with_context(|| format!("Cannot create {}", t.dir.display()))?;
    if let Some(old) = &plan.import {
        m.import(old, paths, progress)?;
    }
    if plan.switch_hint {
        // An upgrade never changes the account by itself; say how to switch.
        progress.line(
            Step::Account,
            &format!("  The service runs as LocalSystem (kept). To switch: Install.cmd --account \"{DEFAULT_SERVICE_ACCOUNT}\""),
        );
    }

    // ---- stop, copy -------------------------------------------------------
    if t.service_exe.is_some() {
        progress.line(Step::Stop, "  Stopping the service…");
        m.stop_service().context("The running service would not stop")?;
    }
    let had_previous = if t.in_place { false } else { m.copy_executable(&t.source_exe, &t.target_exe, progress)? };
    if !t.in_place {
        m.copy_release(&t.source_dir, paths, progress)?;
    }
    m.ensure_dirs(paths).context("Cannot create the data, temp and log folders")?;
    if plan.create_watchfolder {
        m.create_watchfolder(&plan.watchfolder);
    }
    if plan.write_config {
        m.save_config(&plan.config, &paths.config).context("Cannot write config.json")?;
        progress.line(Step::Config, &format!("  Configuration written to {}", paths.config.display()));
    }

    // ---- service, rights, firewall -----------------------------------------
    if plan.account.registers() {
        if t.service_exe.is_some() {
            m.uninstall_service().context("Cannot remove the old service registration")?;
        }
        m.install_service(&t.target_exe, account.as_ref()).context("Cannot register the service")?;
        progress.line(
            Step::Register,
            &format!(
                "  Service registered: automatic (delayed) start, restarts after a failure, runs as {}.",
                account_label(account.as_ref())
            ),
        );
        m.apply_acls(plan, account.as_ref(), progress).await;
    }
    if let Some(port) = plan.firewall {
        m.open_firewall(&t.target_exe, port, progress).await;
    }

    // ---- start and verify --------------------------------------------------
    progress.line(
        Step::Start,
        "  Starting the service and waiting for it to answer (first start can take a few minutes)…",
    );
    m.start_service().context("The service would not start")?;
    match m.wait_for_health(plan.health, plan.tls, HEALTH_WAIT) {
        Some(body) => {
            progress.line(Step::Health, "");
            progress.line(Step::Health, "✓ OmniDownloader is running.");
            report.health = health_lines(&body);
            for l in &report.health {
                progress.line(Step::Health, l);
            }
        }
        None if t.is_upgrade() && had_previous => {
            progress.line(Step::Rollback, "");
            progress.line(
                Step::Rollback,
                &format!("✗ The new version did not answer within {} s. Putting the previous one back…", HEALTH_WAIT.as_secs()),
            );
            let _ = m.stop_service();
            m.restore_previous(&t.target_exe).context("Rollback failed")?;
            m.start_service().context("The previous version would not start either")?;
            report.rolled_back = true;
            return Ok(report);
        }
        None => {
            bail!(
                "The service did not answer on {} within {} s. See {} and Event Viewer → Windows Logs → Application.",
                plan.health,
                HEALTH_WAIT.as_secs(),
                paths.logs.display()
            );
        }
    }

    // ---- what next ---------------------------------------------------------
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this-pc".into());
    for l in closing_lines(plan, &host) {
        progress.line(Step::Done, &l);
    }
    Ok(report)
}

/// What the operator does next. A new installation is finished in the browser,
/// on this computer: the setup wizard creates the administrator and asks for the
/// watchfolder and the mailbox.
fn closing_lines(plan: &InstallPlan, host: &str) -> Vec<String> {
    let t = &plan.target;
    let scheme = if plan.tls { "https" } else { "http" };
    let port = plan.port;
    let mut lines = vec![
        String::new(),
        format!("  MCR desk:  {scheme}://{host}:{port}/mcr"),
        format!("  Admin:     {scheme}://{host}:{port}/admin"),
        format!("  Files:     {}", t.paths.root.display()),
        format!("  Watchfolder: {}", plan.watchfolder.display()),
    ];
    if t.is_upgrade() {
        lines.push(format!("  Previous version kept as {}.", t.target_exe.with_extension("exe.prev").display()));
    } else {
        lines.push(String::new());
        lines.push(format!("  Next: on THIS computer, open {scheme}://127.0.0.1:{port}/admin and sign in"));
        lines.push("  as Admin / Admin, the first-run account, which works only from this computer.".into());
        lines.push("  The setup wizard then opens by itself; it replaces that account with your own".into());
        lines.push("  administrator and walks you through the watchfolder and the mailbox.".into());
        lines.push("  To let the MCR desks open the panel without signing in, choose Intermediate".into());
        lines.push("  in the wizard and enter the newsroom network in its Network step (you can".into());
        lines.push("  run the wizard again later from the admin page).".into());
        lines.push("  A watchfolder on a NAS or an IP-addressed share needs the service to run".into());
        lines.push("  under an account; the wizard tells you what to do.".into());
    }
    lines
}

/// The console installer: plan, ask the plan's questions, execute.
pub async fn install(o: InstallOptions) -> Result<()> {
    let report = install_console(&o, &SystemProbe, &mut ConsoleProgress).await?;
    if report.rolled_back {
        bail!(
            "Upgrade rolled back; the previous version is running. See {} for why the new one failed.",
            report.logs.display()
        );
    }
    Ok(())
}

/// [`install`] with the probe and the output given, and a rolled-back upgrade
/// returned in the report rather than as an error: locate, plan, ask the
/// plan's questions at the console (`inquire`), execute. Setup (P13.1b)
/// runs it with the release it carries as the probe's executable.
pub async fn install_console(o: &InstallOptions, probe: &dyn Probe, out: &mut dyn Progress) -> Result<InstallReport> {
    let target = locate(o, probe)?;
    for l in target.header() {
        out.line(Step::Plan, &l);
    }
    let console_plan = |answers: &Answers, out: &mut dyn Progress| {
        plan_from(target.clone(), o, answers, probe).inspect_err(|e| {
            if let Some(r) = e.downcast_ref::<Refusal>() {
                out.line(Step::Account, "");
                for l in &r.notice {
                    out.line(Step::Account, l);
                }
            }
        })
    };
    let mut answers = Answers::default();
    let mut plan = console_plan(&answers, out)?;
    if let Some(default) = plan.needs_watchfolder() {
        let answer = answered(
            Text::new("Dalet watchfolder (local folder or \\\\server\\share):").with_default(default).prompt(),
            "No watchfolder given",
        )?;
        answers.watchfolder = Some(answer);
        plan = console_plan(&answers, out)?;
    }
    for q in &plan.questions {
        match q {
            Question::NeedWatchfolder { .. } => bail!("No watchfolder given"),
            Question::ConfirmUncWithoutAccount { text, prompt } => {
                out.line(Step::Account, "");
                for l in text {
                    out.line(Step::Account, l);
                }
                let go = answered(inquire::Confirm::new(prompt).with_default(false).prompt(), "No answer given")?;
                if !go {
                    return Err(Cancelled.into());
                }
                answers.confirm_unc = true;
            }
            Question::NeedPassword { account } => {
                let password = answered(
                    inquire::Password::new(&format!("Password for {account}:")).without_confirmation().prompt(),
                    "No password given",
                )?;
                answers.password = Some(password);
            }
        }
    }
    execute(&plan, &answers, out).await
}

/// Remove the service and the firewall rule; leave every file in place.
pub async fn uninstall(dir: Option<PathBuf>) -> Result<()> {
    if !can_install_services() {
        bail!("Uninstalling needs administrator rights (run Uninstall.cmd or an elevated terminal).");
    }
    remove_service(&mut ConsoleProgress).await?;
    let dir = dir.unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    println!(
        "  The files are still in {} (database, configuration, logs). Delete that folder to remove them too.",
        dir.display()
    );
    Ok(())
}

/// Stop and remove the registered service, then the firewall rule; every
/// line goes to `progress`. Shared by `uninstall` and Setup's `/uninstall`.
pub async fn remove_service(progress: &mut dyn Progress) -> Result<()> {
    remove_service_registration(progress)?;
    remove_firewall_rule().await;
    Ok(())
}

/// Stop and remove the registered service only.
pub fn remove_service_registration(progress: &mut dyn Progress) -> Result<()> {
    if installed_service_exe().is_some() {
        let _ = stop_service_and_wait(Duration::from_secs(120));
        uninstall_service()?;
        progress.line(Step::Register, "✓ Service removed.");
    } else {
        progress.line(Step::Register, "  The service was not installed.");
    }
    Ok(())
}

/// Remove the panel's firewall rule; a missing rule is fine.
pub async fn remove_firewall_rule() {
    let _ = netsh(&["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).await;
}

/// Every file or folder the installer can have put into `root` itself, for
/// an uninstaller that removes exactly those: the executable with its
/// `.prev` and staged `.exe.new` copies (`copy_executable`) and the tools in
/// `bin` with the DLLs found there (`copy_tools`). The paths may not exist.
/// Not the seeds in `data` (the station's roster and taxonomy once the
/// service has read them), `config.json`, the database or anything else the
/// service writes.
pub fn program_files(root: &Path) -> Vec<PathBuf> {
    let paths = AppPaths::with_root(root, "config.json");
    let exe = paths.root.join(EXE);
    let mut files = vec![exe.with_extension("exe.prev"), exe.with_extension("exe.new"), exe];
    files.extend(TOOLS.iter().chain(SELF_UPDATING).map(|t| paths.bin.join(t)));
    if let Ok(entries) = std::fs::read_dir(&paths.bin) {
        // DLLs, and what `replace_file` left half-done (`*.omni-new`).
        let mut more: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                n.ends_with(".dll") || (n.ends_with(&format!(".{STAGED}")) && !files.contains(&paths.bin.join(e.file_name())))
            })
            .map(|e| paths.bin.join(e.file_name()))
            .collect();
        more.sort();
        files.extend(more);
    }
    // A seed staged but never renamed into place is installer debris, not data.
    files.extend(SEEDS.iter().map(|s| staged_name(&paths.data.join(s))));
    files
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
        let who = choice_name(&choice);
        println!();
        for l in share_warning(watchfolder, who) {
            println!("{l}");
        }
        if yes {
            bail!(SHARE_REFUSAL);
        }
        let go = answered(inquire::Confirm::new(&format!("Install as {who} anyway?")).with_default(false).prompt(), "No answer given")?;
        if !go {
            return Err(Cancelled.into());
        }
    }
    match choice {
        AccountChoice::LocalSystem => Ok(None),
        AccountChoice::Passwordless(name) => Ok(Some(ServiceAccount { name, password: None })),
        AccountChoice::NeedsPassword(name) => {
            let password = answered(
                inquire::Password::new(&format!("Password for {name}:")).without_confirmation().prompt(),
                "No password given",
            )?;
            Ok(Some(ServiceAccount::with_password(name, password)))
        }
    }
}

/// The account a choice names, as the share warning says it.
fn choice_name(choice: &AccountChoice) -> &str {
    match choice {
        AccountChoice::Passwordless(n) | AccountChoice::NeedsPassword(n) => n.as_str(),
        AccountChoice::LocalSystem => "LocalSystem",
    }
}

/// Why a network watchfolder and a computer-identity account fail at delivery.
fn share_warning(watchfolder: &str, who: &str) -> Vec<String> {
    vec![
        format!("! The watchfolder {watchfolder} is a network share. As {who} the service"),
        "  reaches it as this computer's account, which most file servers refuse; files".into(),
        "  then fail at the very last step. Either install with a domain account that".into(),
        "  has Modify on the share:   Install.cmd --account \"DOMAIN\\svc_omni\"".into(),
        "  or grant this computer's account (DOMAIN\\THIS-PC$) Modify on the share.".into(),
    ]
}

/// How the service account is told to the operator.
fn account_label(account: Option<&ServiceAccount>) -> &str {
    account.map(|a| a.name.as_str()).unwrap_or("LocalSystem")
}

/// Which rights the installer gives the service account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rights {
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
    grant_plan_with(paths, config, named_account, &omni_core::paths::SystemDrives)
}

/// [`grant_plan`] with the drive letters looked up through `drives`.
pub(crate) fn grant_plan_with(
    paths: &AppPaths,
    config: &AppConfig,
    named_account: bool,
    drives: &dyn DriveLookup,
) -> Vec<(PathBuf, Rights)> {
    let mut plan: Vec<(PathBuf, Rights)> = Vec::new();
    let mut add = |p: PathBuf, r: Rights| {
        if !plan.contains(&(p.clone(), r)) {
            plan.push((p, r));
        }
    };
    add(paths.root.clone(), Rights::LockRoot);
    let watch = paths.resolve(&config.watchfolder_path);
    let local_watch = !needs_network_identity_with(&config.watchfolder_path, drives);
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
/// service runs as; `same_dir` compares two paths canonically (the probe's).
pub(crate) fn account_plan_with(
    installed_exe: Option<&Path>,
    target_exe: &Path,
    account_arg: Option<&str>,
    current_account: Option<&str>,
    same_dir: &dyn Fn(&Path, &Path) -> bool,
) -> AccountPlan {
    let asked = account_arg.map_or(false, |s| !s.trim().is_empty());
    let register = installed_exe.map_or(true, |p| !same_file_with(p, target_exe, same_dir)) || asked;
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
    let grants = grant_plan(paths, config, account.is_some());
    apply_acls_with(paths, config, account, previous_account, service_exe, &grants, &mut ConsoleProgress).await;
}

/// [`apply_acls`] with the grants already planned and the lines sent to `progress`.
pub(crate) async fn apply_acls_with(
    paths: &AppPaths,
    config: &AppConfig,
    account: Option<&ServiceAccount>,
    previous_account: Option<&str>,
    service_exe: &Path,
    grants: &[(PathBuf, Rights)],
    progress: &mut dyn Progress,
) {
    let lock_root = is_installed_root(service_exe, &paths.root);
    apply_acls_on(paths, config, account, previous_account, lock_root, grants, progress, &mut SystemAcl).await
}

/// The permission changes of [`apply_acls_with`], one method per icacls run, so
/// which paths get one is tested without changing the machine.
pub(crate) trait AclOps {
    fn is_link(&self, path: &Path) -> bool;
    /// The first link at or below `path`, never entering one (`fsutil::first_link_in_tree`).
    fn first_link_in_tree(&self, path: &Path) -> Option<PathBuf>;
    /// The entries directly in `dir`.
    fn entries(&self, dir: &Path) -> Vec<PathBuf>;
    async fn remove_grants(&mut self, path: &Path, account: &str, recursive: bool) -> Result<()>;
    async fn lock_root(&mut self, root: &Path, account: Option<&str>) -> Result<()>;
    /// Create `path` when `rights` is a folder grant, then grant.
    async fn grant_modify(&mut self, account: &str, path: &Path, rights: Rights, progress: &mut dyn Progress) -> bool;
    async fn lock_dirs(&mut self, dirs: &[PathBuf], account: omni_core::lockdown::Account) -> Result<()>;
}

struct SystemAcl;

impl AclOps for SystemAcl {
    fn is_link(&self, path: &Path) -> bool {
        omni_core::fsutil::is_link(path)
    }
    fn first_link_in_tree(&self, path: &Path) -> Option<PathBuf> {
        omni_core::fsutil::first_link_in_tree(path, &omni_core::fsutil::is_link)
    }
    fn entries(&self, dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        v.sort();
        v
    }
    async fn remove_grants(&mut self, path: &Path, account: &str, recursive: bool) -> Result<()> {
        omni_core::lockdown::remove_grants_with(path, account, recursive).await
    }
    async fn lock_root(&mut self, root: &Path, account: Option<&str>) -> Result<()> {
        omni_core::lockdown::lock_root(root, account).await
    }
    async fn grant_modify(&mut self, account: &str, path: &Path, rights: Rights, progress: &mut dyn Progress) -> bool {
        grant_modify(account, path, rights, progress).await
    }
    async fn lock_dirs(&mut self, dirs: &[PathBuf], account: omni_core::lockdown::Account) -> Result<()> {
        omni_core::lockdown::apply(dirs, account).await
    }
}

/// A link that an elevated, recursive permission change on `path` would follow:
/// on the way down from `root` (for a path inside it) or at `path`, or anywhere
/// in its tree (P13.1b, S1). The service account may write these folders.
fn link_in<A: AclOps>(ops: &A, root: &Path, path: &Path) -> Option<PathBuf> {
    let on_the_way = if path.starts_with(root) {
        omni_core::fsutil::link_below(root, path, &|p| ops.is_link(p))
    } else {
        ops.is_link(path).then(|| path.to_path_buf())
    };
    on_the_way.or_else(|| ops.first_link_in_tree(path))
}

fn link_line(path: &Path, link: &Path) -> String {
    format!(
        "✗ {} contains a link ({}): its permissions were NOT changed. Remove the link and run the installer again.",
        path.display(),
        link.display()
    )
}

pub(crate) async fn apply_acls_on<A: AclOps>(
    paths: &AppPaths,
    config: &AppConfig,
    account: Option<&ServiceAccount>,
    previous_account: Option<&str>,
    // False for a development tree (the exe in `target\release`, the root the repo): it is
    // never locked, which would take write access from the developer's own account (UAC).
    lock_root: bool,
    grants: &[(PathBuf, Rights)],
    progress: &mut dyn Progress,
    ops: &mut A,
) {
    let named = account.map(|a| a.name.as_str());
    let root = &paths.root;
    let temp = paths.resolve(&config.temp_path);
    if !lock_root {
        progress.line(
            Step::Rights,
            &format!("  {} is not an installed root (development tree); not restricting it.", root.display()),
        );
    }
    // A named account without rights on data\ or temp\ cannot start.
    let mut service_blocked = false;
    // 1. An older installer granted the account Modify on the whole root and every file.
    let mut stale: Vec<&str> = Vec::new();
    for a in [previous_account, named].into_iter().flatten() {
        if !stale.iter().any(|s| s.eq_ignore_ascii_case(a)) {
            stale.push(a);
        }
    }
    let root_link = if lock_root && !stale.is_empty() { ops.first_link_in_tree(root) } else { None };
    for a in stale.into_iter().filter(|_| lock_root) {
        let result = match &root_link {
            None => ops.remove_grants(root, a, true).await,
            // `/T` would follow the link: the root alone, then each link-free entry.
            Some(_) => {
                let mut r = ops.remove_grants(root, a, false).await;
                for entry in ops.entries(root) {
                    match link_in(ops, root, &entry) {
                        None => {
                            if let Err(e) = ops.remove_grants(&entry, a, true).await {
                                r = Err(e);
                            }
                        }
                        Some(link) => progress.line(Step::Rights, &link_line(&entry, &link)),
                    }
                }
                r
            }
        };
        if let Err(e) = result {
            progress.line(Step::Rights, &format!("! Could not remove the old rights of {a} ({e:#}); set it by hand."));
        }
    }
    // 2. Root, then the folders the daemon writes.
    for (path, rights) in grants.iter().cloned() {
        if rights == Rights::LockRoot && !lock_root {
            continue;
        }
        if rights != Rights::ModifyAuthenticatedUsers && rights != Rights::LockRoot && named.is_none() {
            continue;
        }
        if let Some(link) = link_in(ops, root, &path) {
            progress.line(Step::Rights, &link_line(&path, &link));
            if named.is_some() && (path == paths.data || path == temp) {
                service_blocked = true;
            }
            continue;
        }
        match (rights, named) {
            (Rights::LockRoot, _) => match ops.lock_root(&path, named).await {
                Ok(()) => progress.line(
                    Step::Rights,
                    "  Install folder restricted: Users may read, only SYSTEM and Administrators may change it.",
                ),
                Err(e) => progress.line(Step::Rights, &format!("! Could not restrict the install folder ({e:#}); set it by hand.")),
            },
            (Rights::ModifyAuthenticatedUsers, _) => {
                ops.grant_modify(AUTHENTICATED_USERS, &path, Rights::ModifyDir, progress).await;
            }
            (_, Some(name)) => {
                let ok = ops.grant_modify(name, &path, rights, progress).await;
                if !ok && (path == paths.data || path == temp) {
                    service_blocked = true;
                }
            }
            _ => {}
        }
    }
    // 3. data\ and temp\ hold the sealed secrets and cookie jars (P3.5): after the
    //    grants, so the service account keeps Modify on them.
    let account_arg = match account {
        Some(a) => omni_core::lockdown::Account::Named(a.name.clone()),
        None => omni_core::lockdown::Account::System,
    };
    let mut dirs = Vec::new();
    for d in [paths.data.clone(), temp] {
        match link_in(ops, root, &d) {
            None => dirs.push(d),
            Some(link) => {
                if !grants.iter().any(|(p, _)| *p == d) || named.is_none() {
                    progress.line(Step::Rights, &link_line(&d, &link));
                }
                service_blocked |= named.is_some();
            }
        }
    }
    if service_blocked {
        progress.line(
            Step::Rights,
            &format!(
                "! The service will NOT start: {} has no access to data\\ or temp\\. Grant it Modify there, then start the service.",
                account_label(account)
            ),
        );
    }
    if dirs.is_empty() {
        return;
    }
    match ops.lock_dirs(&dirs, account_arg).await {
        Ok(()) => progress.line(
            Step::Rights,
            "  data and temp folders restricted to SYSTEM, Administrators and the service account.",
        ),
        Err(e) => progress.line(Step::Rights, &format!("! Could not restrict the data and temp folders ({e:#}); set it by hand.")),
    }
}

/// [`AppPaths::ensure_dirs`] for the installer: nothing is created through a
/// link below the root (P13.1b, S1).
fn ensure_dirs_with(paths: &AppPaths, is_link: &dyn Fn(&Path) -> bool) -> Result<()> {
    let dirs = [
        paths.data.clone(),
        paths.temp.clone(),
        paths.logs.clone(),
        paths.archive.clone(),
        paths.temp.join("jobs"),
        paths.data.join("adblock"),
    ];
    for d in &dirs {
        if let Some(link) = omni_core::fsutil::link_below(&paths.root, d, is_link) {
            bail!("{} is reached through a link ({}); Setup does not create folders through it. Remove the link and run Setup again.", d.display(), link.display());
        }
    }
    for d in &dirs {
        std::fs::create_dir_all(d).with_context(|| format!("Cannot create {}", d.display()))?;
    }
    Ok(())
}

/// Which descriptor a new install folder and its missing parents get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewDirRights {
    /// [`omni_core::lockdown::INSTALL_ROOT_SDDL`] from the moment it exists.
    InstallRoot,
    /// Inherited as usual: a development tree, which is never locked.
    Plain,
}

/// The rights a new root (and each missing parent) is created with (S5): only
/// a cargo build run in place ([`Target::development`]) is created plainly.
pub(crate) fn new_dir_rights(development: bool) -> NewDirRights {
    if development {
        NewDirRights::Plain
    } else {
        NewDirRights::InstallRoot
    }
}

/// What execute-time checks read from the machine (T2); tests inject them.
pub(crate) struct Recheck<'a> {
    pub is_dir: &'a dyn Fn(&Path) -> bool,
    pub is_link: &'a dyn Fn(&Path) -> bool,
    /// The owner's name when it is not trusted.
    pub foreign_owner: &'a dyn Fn(&Path) -> Option<String>,
    /// Who besides the trusted accounts may rename, re-permission or owns a folder.
    pub swapper: &'a dyn Fn(&Path) -> Option<String>,
    /// Who besides the trusted accounts may put files into a folder (U3).
    pub writable: &'a dyn Fn(&Path) -> Option<String>,
}

impl Recheck<'static> {
    /// The real machine.
    pub(crate) fn system() -> Self {
        Recheck {
            is_dir: &Path::is_dir,
            is_link: &omni_core::fsutil::is_link,
            foreign_owner: &omni_core::lockdown::foreign_owner,
            swapper: &omni_core::lockdown::folder_swapper,
            writable: &omni_core::lockdown::writable_by,
        }
    }
}

/// What changed about the root's existing path since `locate` judged it (the
/// prompts lie in between): a component that became a link; the root, a new
/// owner; a parent below the volume root, a new owner or swap right (unless an
/// installation is already in the folder, as in `locate`). `None`: unchanged.
pub(crate) fn changed_since_plan(t: &Target, r: &Recheck) -> Option<String> {
    let root = &t.paths.root;
    let changed = |p: &Path, why: String| {
        Some(format!("{} changed while Setup was running: {why}; nothing was installed.", p.display()))
    };
    for a in root.ancestors().filter(|a| (r.is_dir)(a)) {
        if (r.is_link)(a) {
            return changed(a, "it is now a link".into());
        }
        if t.development {
            continue;
        }
        if a == root.as_path() {
            if let Some(owner) = (r.foreign_owner)(a) {
                return changed(a, format!("it now belongs to {owner}"));
            }
            // As in `locate` (U3): a folder that is no installation must stay closed.
            if !t.installed_here() {
                if let Some(who) = (r.writable)(a) {
                    return changed(a, format!("{who} can now write to it"));
                }
            }
        } else if a.parent().is_some() && !t.installed_here() {
            if let Some(who) = (r.swapper)(a) {
                return changed(a, format!("{who} may rename or re-permission it"));
            }
        }
    }
    None
}

/// The install folder, after [`changed_since_plan`]. An existing one is used
/// as it is. A new one, and every missing parent, gets the P2.16 baseline from
/// the moment it exists (`lockdown::create_install_root_dir`: Administrators
/// and SYSTEM Full, Users Read & execute), so nothing can be planted in it and
/// nobody can swap it before the lockdown step; the service account can still
/// run the executable if that step fails. A cargo build in place is created plainly.
fn create_install_dir(t: &Target) -> Result<()> {
    if new_dir_rights(t.development) == NewDirRights::Plain {
        let recheck = Recheck::system();
        if let Some(message) = changed_since_plan(t, &recheck) {
            return Err(Refusal { notice: Vec::new(), message }.into());
        }
        return Ok(std::fs::create_dir_all(&t.dir)?);
    }
    create_install_dir_with(t, &Recheck::system(), &mut |d: &Path| omni_core::lockdown::create_install_root_dir(d))
}

/// [`create_install_dir`] with the machine injected. What exists and what is
/// missing is read BEFORE the re-check (U1): only a root that existed then is
/// used as it is; every missing folder (parents and root) must be created by
/// this run, and one that appears in between is refused, never adopted.
pub(crate) fn create_install_dir_with(
    t: &Target,
    r: &Recheck,
    create: &mut dyn FnMut(&Path) -> std::io::Result<()>,
) -> Result<()> {
    let dir = t.dir.as_path();
    let existed = (r.is_dir)(dir);
    let mut missing: Vec<PathBuf> = dir.ancestors().take_while(|a| !(r.is_dir)(a)).map(Path::to_path_buf).collect();
    missing.reverse();
    if let Some(message) = changed_since_plan(t, r) {
        return Err(Refusal { notice: Vec::new(), message }.into());
    }
    if existed {
        return Ok(());
    }
    for d in &missing {
        match create(d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let why = match (r.foreign_owner)(d) {
                    Some(owner) => format!("it appeared and belongs to {owner}"),
                    None => match (d != dir).then(|| (r.swapper)(d)).flatten() {
                        Some(who) => format!("it appeared and {who} may rename or re-permission it"),
                        None => "it appeared while Setup was creating it".into(),
                    },
                };
                let message = format!("{} changed while Setup was running: {why}; nothing was installed.", d.display());
                return Err(Refusal { notice: Vec::new(), message }.into());
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("Cannot create {}", d.display()))),
        }
    }
    Ok(())
}

/// Replace the installed executable with this one, keeping the old one as
/// `.prev`. Returns whether there was an old one.
fn copy_executable(source: &Path, target: &Path, progress: &mut dyn Progress) -> Result<bool> {
    let prev = target.with_extension("exe.prev");
    let had = target.exists();
    if had {
        std::fs::copy(target, &prev).with_context(|| format!("Cannot keep the previous version as {}", prev.display()))?;
    }
    let staged = target.with_extension("exe.new");
    std::fs::copy(source, &staged).with_context(|| format!("Cannot copy to {}", staged.display()))?;
    std::fs::rename(&staged, target).with_context(|| format!("Cannot install {}", target.display()))?;
    progress.line(Step::Copy, &format!("  Installed {}.", target.display()));
    Ok(had)
}

/// Refuse to write into `dir`, or onto `file` in it, when either is a link: the
/// service account may write `bin` and `data`, and this runs elevated (P13.1b, R2).
fn refuse_links(dir: &Path, file: Option<&Path>, is_link: &dyn Fn(&Path) -> bool) -> Result<()> {
    omni_core::fsutil::ensure_not_link(dir, is_link)?;
    if let Some(f) = file {
        omni_core::fsutil::ensure_not_link(f, is_link)?;
    }
    Ok(())
}

/// The release's tools into `bin/`: replaced when different, self-updating
/// ones only when missing, DLLs alongside.
fn copy_tools(from: &Path, to: &Path, progress: &mut dyn Progress) -> Result<()> {
    copy_tools_with(from, to, progress, &omni_core::fsutil::is_link)
}

fn copy_tools_with(from: &Path, to: &Path, progress: &mut dyn Progress, is_link: &dyn Fn(&Path) -> bool) -> Result<()> {
    if !from.is_dir() {
        progress.line(Step::Copy, "  (No bin/ in this release folder: tools left as they are.)");
        return Ok(());
    }
    refuse_links(to, None, is_link)?;
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let src = entry.path();
        let dst = to.join(&name);
        refuse_links(to, Some(&dst), is_link)?;
        let is_tool = TOOLS.contains(&name.as_str()) || name.to_ascii_lowercase().ends_with(".dll");
        if SELF_UPDATING.contains(&name.as_str()) {
            if !dst.exists() {
                copy_recursive(&src, &dst)?;
                progress.line(Step::Copy, &format!("  Installed bin\\{name}."));
            }
        } else if is_tool && differs(&src, &dst) {
            replace_file(&src, &dst).with_context(|| format!("Cannot install bin\\{name}"))?;
            progress.line(Step::Copy, &format!("  Installed bin\\{name}."));
        }
    }
    Ok(())
}

/// The suffix of a file being put in place by [`replace_file`].
pub(crate) const STAGED: &str = "omni-new";

/// `<name>.omni-new` beside `dst`.
fn staged_name(dst: &Path) -> PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{STAGED}"));
    dst.with_file_name(name)
}

/// Put `src` in place of `dst` by writing a new file beside it and renaming it
/// over: a hard link at `dst` (made by the account that may write the folder)
/// is replaced, never written through (S9). The staged file is created fresh
/// (an existing one is removed first, a link at it included) and removed on
/// any failure.
fn replace_file(src: &Path, dst: &Path) -> Result<()> {
    let staged = staged_name(dst);
    match std::fs::remove_file(&staged) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let result = (|| -> Result<()> {
        let mut input = std::fs::File::open(src)?;
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&staged)?;
        std::io::copy(&mut input, &mut out)?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&staged, dst)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

fn copy_missing(from: &Path, to: &Path, names: &[&str], progress: &mut dyn Progress) -> Result<()> {
    copy_missing_with(from, to, names, progress, &omni_core::fsutil::is_link)
}

fn copy_missing_with(
    from: &Path,
    to: &Path,
    names: &[&str],
    progress: &mut dyn Progress,
    is_link: &dyn Fn(&Path) -> bool,
) -> Result<()> {
    for name in names {
        let src = from.join(name);
        let dst = to.join(name);
        refuse_links(to, Some(&dst), is_link)?;
        if src.is_file() && !dst.exists() {
            std::fs::create_dir_all(to)?;
            replace_file(&src, &dst).with_context(|| format!("Cannot install data\\{name}"))?;
            progress.line(Step::Copy, &format!("  Installed data\\{name}."));
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

/// Whether `root` is an installed root: the service's executable sits directly in it and is
/// not a cargo build ([`is_cargo_build`]: `<repo>\target\release\omni-ingest.exe`, whose root
/// is the repo or the profile folder). Judged on the executable, never on a `Cargo.toml`
/// near the root, which anyone may write (T1).
pub(crate) fn is_installed_root(service_exe: &Path, root: &Path) -> bool {
    service_exe.parent().is_some_and(|dir| same_dir(dir, root)) && !is_cargo_build(service_exe, &|p| p.is_file())
}

/// Whether two paths name the same existing file or folder (canonical, case-folded).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    match (dunce(a), dunce(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn same_file_with(a: &Path, b: &Path, same_dir: &dyn Fn(&Path, &Path) -> bool) -> bool {
    same_dir(a, b) || a.to_string_lossy().trim_matches('"').eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Canonical, case-folded, without the `\\?\` prefix Windows adds.
fn dunce(p: &Path) -> Option<String> {
    let c = std::fs::canonicalize(p).ok()?;
    Some(c.to_string_lossy().trim_start_matches(r"\\?\").to_ascii_lowercase())
}

/// The service account gets Modify on what it writes.
async fn grant_modify(account: &str, path: &Path, rights: Rights, progress: &mut dyn Progress) -> bool {
    if rights == Rights::ModifyDir {
        let _ = std::fs::create_dir_all(path);
    }
    let args = [path.as_os_str().to_os_string(), "/grant".into(), grant_spec(account, rights).into(), "/T".into(), "/Q".into()];
    let icacls = match omni_core::lockdown::icacls() {
        Ok(p) => p,
        Err(e) => {
            progress.line(Step::Rights, &format!("! icacls.exe was not found ({e}); give {account} Modify on {} by hand.", path.display()));
            return false;
        }
    };
    match omni_core::process::run_capture(&icacls, args, Duration::from_secs(120)).await {
        Ok(o) if o.success => {
            progress.line(Step::Rights, &format!("  {account} may modify {}.", path.display()));
            true
        }
        _ => {
            progress.line(Step::Rights, &format!("! Could not give {account} Modify on {}; set it by hand.", path.display()));
            false
        }
    }
}

/// `netsh.exe` by its full path: this runs elevated, and a bare name is looked
/// up beside the running executable first. Never a bare name.
fn netsh_program() -> std::io::Result<PathBuf> {
    omni_core::paths::system_tool("netsh.exe")
}

/// `false` when netsh failed or cannot be found; the callers say what that means.
async fn netsh(args: &[&str]) -> bool {
    let Ok(program) = netsh_program() else { return false };
    matches!(
        omni_core::process::run_capture(&program, args, Duration::from_secs(30)).await,
        Ok(o) if o.success
    )
}

/// One inbound rule for this executable on the panel's port, on the domain
/// and private networks (not public Wi-Fi). Replaced on every install.
async fn open_firewall(exe: &Path, port: u16, progress: &mut dyn Progress) {
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
        progress.line(Step::Firewall, &format!("  Firewall: TCP {port} open on domain and private networks."));
    } else {
        progress.line(Step::Firewall, &format!("! Could not add the firewall rule; the desks may not reach port {port}."));
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

/// The summary of a `/api/health` answer; nothing when it is not JSON.
fn health_lines(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut lines = vec![format!("  Health: {}", v["status"].as_str().unwrap_or("?"))];
    if let Some(checks) = v["checks"].as_object() {
        for (name, state) in checks {
            let s = state.as_str().unwrap_or("?");
            let mark = if s == "ok" { "✓" } else { "!" };
            lines.push(format!("    {mark} {name}: {s}"));
        }
    }
    lines
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

    // The pre-split signatures (P13.1a), so the tests written against them read as before.
    fn account_plan(i: Option<&Path>, t: &Path, a: Option<&str>, c: Option<&str>) -> AccountPlan {
        account_plan_with(i, t, a, c, &same_dir)
    }
    fn copy_tools_plain(from: &Path, to: &Path) -> Result<()> {
        super::copy_tools(from, to, &mut Log::default())
    }
    fn copy_executable(source: &Path, target: &Path) -> Result<bool> {
        super::copy_executable(source, target, &mut Log::default())
    }
    fn import_from(old: &Path, new: &AppPaths) -> Result<AppConfig> {
        super::import_from(old, new, &mut Log::default())
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
        // Another cargo output, where the discovered root is the profile folder itself: a
        // `target` folder whose parent is a checkout (T1: judged on the executable).
        std::fs::write(repo.join("Cargo.toml"), "").unwrap();
        let gate = dir.path().join("repo").join("target").join("gate").join("release");
        std::fs::create_dir_all(&gate).unwrap();
        assert!(!is_installed_root(&gate.join("omni-ingest.exe"), &gate), "target\\gate\\release is a build, not an install");
        // T1: an exe copied to the top of a checkout is not a cargo build, so it is locked: a
        // `Cargo.toml` beside the root, which anyone may write, never switches the lock off.
        assert!(is_installed_root(&repo.join("omni-ingest.exe"), &repo));
        // Without the checkout's Cargo.toml a folder named `target` is just a folder.
        std::fs::remove_file(repo.join("Cargo.toml")).unwrap();
        assert!(is_installed_root(&gate.join("omni-ingest.exe"), &gate));
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

        copy_tools_plain(&from, &to).unwrap();
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

    // ---- P13.1a: the plan, the execution and the console lines ------------------

    use std::cell::RefCell;
    use std::rc::Rc;

    /// Progress lines and fake machine steps in one ordered list; machine steps start with `#`.
    #[derive(Clone, Default)]
    struct Log(Rc<RefCell<Vec<String>>>);

    impl Log {
        fn push(&self, s: impl Into<String>) {
            self.0.borrow_mut().push(s.into());
        }
        fn lines(&self) -> Vec<String> {
            self.0.borrow().clone()
        }
    }

    impl Progress for Log {
        fn line(&mut self, _step: Step, text: &str) {
            self.push(text);
        }
    }

    const ROOT: &str = r"C:\OmniTest";
    const RELEASE_EXE: &str = r"D:\Release\omni-ingest.exe";
    const PASSWORD: &str = "Tr0ub4dor-test-only";

    /// A machine described by data: files (with sizes), config files, the registered service.
    struct FakeProbe {
        admin: bool,
        files: Vec<(PathBuf, u64)>,
        configs: Vec<(PathBuf, AppConfig)>,
        service: Option<(PathBuf, String)>,
        /// The install folder's owner, when it is not one we trust.
        owner: Option<String>,
        /// Accounts Windows would not find.
        unknown_accounts: Vec<String>,
        /// Paths that are reparse points.
        links: Vec<PathBuf>,
        /// A folder above the root that someone else may swap.
        swappable: Option<(PathBuf, String)>,
        /// The executable being installed, when not the release folder's.
        exe: Option<PathBuf>,
        /// Existing folders.
        dirs: Vec<PathBuf>,
        /// Files whose owner is not trusted.
        untrusted: Vec<PathBuf>,
        /// Who may write into an existing root.
        writable: Option<String>,
        /// Every tree walk asked for.
        walks: std::cell::RefCell<Vec<PathBuf>>,
    }

    fn key(p: &Path) -> String {
        p.to_string_lossy().to_ascii_lowercase()
    }

    impl Probe for FakeProbe {
        fn can_install_services(&self) -> bool {
            self.admin
        }
        fn current_exe(&self) -> std::io::Result<PathBuf> {
            Ok(self.exe.clone().unwrap_or_else(|| PathBuf::from(RELEASE_EXE)))
        }
        fn installed_exe(&self) -> Option<PathBuf> {
            self.service.as_ref().map(|s| s.0.clone())
        }
        fn installed_account(&self) -> Option<String> {
            self.service.as_ref().map(|s| s.1.clone())
        }
        fn exists(&self, path: &Path) -> bool {
            self.is_file(path) || self.dirs.iter().any(|d| key(d) == key(path))
        }
        fn is_file(&self, path: &Path) -> bool {
            self.file_len(path).is_some() || self.configs.iter().any(|(p, _)| key(p) == key(path))
        }
        fn file_len(&self, path: &Path) -> Option<u64> {
            self.files.iter().find(|(p, _)| key(p) == key(path)).map(|f| f.1)
        }
        fn load_config(&self, path: &Path) -> Result<AppConfig> {
            match self.configs.iter().find(|(p, _)| key(p) == key(path)) {
                Some((_, c)) => Ok(c.clone()),
                None => bail!("no such config {}", path.display()),
            }
        }
        fn same_dir(&self, a: &Path, b: &Path) -> bool {
            key(a) == key(b)
        }
        fn drives(&self) -> &dyn DriveLookup {
            &MappedZ
        }
        fn foreign_owner(&self, dir: &Path) -> Option<String> {
            self.owner.clone().filter(|_| key(dir) == key(Path::new(ROOT)))
        }
        fn account_exists(&self, name: &str) -> bool {
            !self.unknown_accounts.iter().any(|a| a.eq_ignore_ascii_case(name))
        }
        fn is_link(&self, path: &Path) -> bool {
            self.links.iter().any(|l| key(l) == key(path))
        }
        fn swappable_ancestor(&self, _root: &Path) -> Option<(PathBuf, String)> {
            self.swappable.clone()
        }
        fn first_link_under(&self, dir: &Path) -> Option<PathBuf> {
            self.walks.borrow_mut().push(dir.to_path_buf());
            let dir = PathBuf::from(key(dir));
            self.links.iter().find(|l| PathBuf::from(key(l)).starts_with(&dir)).cloned()
        }
        fn trusted_owner(&self, path: &Path) -> bool {
            !self.untrusted.iter().any(|u| key(u) == key(path))
        }
        fn writable_by(&self, dir: &Path) -> Option<String> {
            self.writable.clone().filter(|_| key(dir) == key(Path::new(ROOT)))
        }
    }

    fn new_machine() -> FakeProbe {
        FakeProbe {
            admin: true,
            files: Vec::new(),
            configs: Vec::new(),
            service: None,
            owner: None,
            unknown_accounts: Vec::new(),
            links: Vec::new(),
            swappable: None,
            exe: None,
            dirs: Vec::new(),
            untrusted: Vec::new(),
            writable: None,
            walks: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn root_paths() -> AppPaths {
        AppPaths::with_root(ROOT, "config.json")
    }

    /// An existing installation in ROOT: its exe, a config.json, the service registered there.
    fn installed(account: &str) -> FakeProbe {
        let paths = root_paths();
        let mut c = AppConfig::default();
        c.watchfolder_path = r"C:\Dalet\ingest".into();
        c.web_port = 8090;
        FakeProbe {
            files: vec![(paths.root.join(EXE), 1)],
            configs: vec![(paths.config.clone(), c)],
            service: Some((paths.root.join(EXE), account.into())),
            ..new_machine()
        }
    }

    fn opts() -> InstallOptions {
        InstallOptions { dir: Some(ROOT.into()), ..Default::default() }
    }

    #[test]
    fn a_fresh_install_with_defaults_asks_nothing_under_yes_and_plans_today_s_steps() {
        let o = InstallOptions { yes: true, ..opts() };
        let plan = plan_install(&o, &new_machine()).unwrap();
        let root = PathBuf::from(ROOT);
        assert_eq!(plan.target.kind, InstallKind::Fresh);
        assert!(!plan.target.in_place);
        assert_eq!(plan.target.target_exe, root.join(EXE));
        assert_eq!(plan.target.source_dir, PathBuf::from(r"D:\Release"));
        assert!(plan.questions.is_empty(), "{:?}", plan.questions);
        assert_eq!(plan.watchfolder_source, WatchfolderSource::Default);
        assert_eq!(plan.config.watchfolder_path, root.join("watchfolder").display().to_string());
        assert_eq!(plan.watchfolder, root.join("watchfolder"));
        assert!(plan.create_watchfolder && plan.write_config);
        assert_eq!(plan.account, AccountDecision::Passwordless(DEFAULT_SERVICE_ACCOUNT.into()));
        assert!(plan.account.registers() && !plan.switch_hint);
        let defaults = AppConfig::default();
        assert_eq!(plan.port, defaults.web_port);
        assert_eq!(plan.firewall, (!is_loopback(&defaults.web_host)).then_some(defaults.web_port));
        assert_eq!(plan.health, health_address(&defaults));
        assert!(!plan.tls);
        assert_eq!(plan.target.header()[2], "  New installation.");
    }

    #[test]
    fn a_fresh_install_without_yes_asks_for_the_watchfolder_then_takes_the_trimmed_answer() {
        let probe = new_machine();
        let plan = plan_install(&opts(), &probe).unwrap();
        let default = PathBuf::from(ROOT).join("watchfolder").display().to_string();
        assert_eq!(plan.questions, vec![Question::NeedWatchfolder { default: default.clone() }]);
        assert_eq!(plan.needs_watchfolder(), Some(default.as_str()));
        // An unanswered plan never reaches the machine.
        let log = Log::default();
        let err = run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).unwrap_err();
        assert!(err.to_string().contains("watchfolder"), "{err}");
        assert!(log.lines().is_empty(), "{:?}", log.lines());

        let answers = Answers { watchfolder: Some(r"  C:\Dalet\in  ".into()), ..Default::default() };
        let plan = plan_answered(&opts(), &answers, &probe).unwrap();
        assert!(plan.questions.is_empty());
        assert_eq!(plan.watchfolder_source, WatchfolderSource::Answered);
        assert_eq!(plan.config.watchfolder_path, r"C:\Dalet\in");
        assert!(plan.write_config);
    }

    #[test]
    fn a_local_watchfolder_from_the_command_line_needs_no_question() {
        let o = InstallOptions { watchfolder: Some(r"C:\Dalet\ingest".into()), ..opts() };
        let plan = plan_install(&o, &new_machine()).unwrap();
        assert!(plan.questions.is_empty());
        assert_eq!(plan.watchfolder_source, WatchfolderSource::Options);
        assert_eq!(plan.watchfolder, PathBuf::from(r"C:\Dalet\ingest"));
        assert!(plan.create_watchfolder && plan.write_config);
    }

    #[test]
    fn a_share_with_a_named_account_asks_its_password_and_registers_it() {
        let o = InstallOptions {
            watchfolder: Some(r"\\dalet\ingest".into()),
            account: Some(r"DOMAIN\svc_test".into()),
            ..opts()
        };
        let plan = plan_install(&o, &new_machine()).unwrap();
        assert_eq!(plan.questions, vec![Question::NeedPassword { account: r"DOMAIN\svc_test".into() }]);
        assert_eq!(plan.account, AccountDecision::NeedsPassword(r"DOMAIN\svc_test".into()));
        assert!(plan.account.registers());
        assert!(!plan.create_watchfolder, "a share is not ours to create");
        assert!(plan.grants.iter().all(|(p, _)| p != Path::new(r"\\dalet\ingest")));
        assert!(plan.grants.contains(&(root_paths().data, Rights::ModifyDir)), "a named account gets its grants");
    }

    #[test]
    fn a_share_with_the_defaulted_account_asks_to_confirm_with_today_s_warning() {
        let o = InstallOptions { watchfolder: Some(r"\\dalet\ingest".into()), ..opts() };
        let plan = plan_install(&o, &new_machine()).unwrap();
        let [Question::ConfirmUncWithoutAccount { text, prompt }] = plan.questions.as_slice() else {
            panic!("{:?}", plan.questions);
        };
        assert_eq!(
            text[0],
            r"! The watchfolder \\dalet\ingest is a network share. As NT SERVICE\OmniIngestService the service"
        );
        assert_eq!(text[3], r#"  has Modify on the share:   Install.cmd --account "DOMAIN\svc_omni""#);
        assert_eq!(text.len(), 5);
        assert_eq!(prompt, r"Install as NT SERVICE\OmniIngestService anyway?");
        // No is the console's default: nothing is touched.
        let log = Log::default();
        let err = run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).unwrap_err();
        assert_eq!(err.to_string(), "Installation cancelled.");
        assert!(err.downcast_ref::<Cancelled>().is_some(), "a cancel is a type, not a text");
        assert!(log.lines().is_empty(), "{:?}", log.lines());
        // Yes goes on with the virtual account.
        let yes = Answers { confirm_unc: true, ..Default::default() };
        let log = Log::default();
        run(&plan, &yes, &log, FakeMachine::new(&log)).unwrap();
        assert!(log.lines().iter().any(|l| l.starts_with(r"#install_service C:\OmniTest\omni-ingest.exe as NT SERVICE\OmniIngestService")));
    }

    #[test]
    fn a_share_with_the_defaulted_account_under_yes_is_refused_with_the_warning() {
        let o = InstallOptions { watchfolder: Some(r"\\dalet\ingest".into()), yes: true, ..opts() };
        let err = plan_install(&o, &new_machine()).unwrap_err();
        assert_eq!(err.to_string(), SHARE_REFUSAL);
        let refusal = err.downcast_ref::<Refusal>().expect("the console prints its notice first");
        assert_eq!(refusal.notice, share_warning(r"\\dalet\ingest", DEFAULT_SERVICE_ACCOUNT));
        // The same refusal as the console-only `choose_account`.
        assert_eq!(choose_account(None, r"\\dalet\ingest", true).unwrap_err().to_string(), SHARE_REFUSAL);
    }

    #[test]
    fn a_mapped_drive_watchfolder_is_refused_with_today_s_text() {
        let o = InstallOptions { watchfolder: Some(r"Z:\Dalet".into()), yes: true, ..opts() };
        let e = plan_install(&o, &new_machine()).unwrap_err();
        assert!(e.downcast_ref::<Refusal>().is_some(), "a refusal, so Setup exits 5 (R12)");
        let err = e.to_string();
        assert!(err.starts_with("Cannot install: "), "{err}");
        assert!(err.contains(r"use \\192.168.4.42\FileIngest\Dalet instead"), "{err}");
    }

    /// R7: after an uninstall that kept the data, the folder still has a
    /// config.json; an answer file must then change nothing at all.
    #[test]
    fn an_answer_file_with_a_config_json_left_in_the_folder_changes_nothing() {
        let mut existing = installed(DEFAULT_SERVICE_ACCOUNT);
        existing.service = None;
        existing.files.clear();
        let kept = existing.configs[0].1.clone();
        let mut answered = AppConfig::default();
        answered.watchfolder_path = r"\\other\ingest".into();
        answered.web_port = 8095;
        answered.setup_mode = "it".into();
        let o = InstallOptions { yes: true, ..opts() }.with_base_config(answered);
        let plan = plan_install(&o, &existing).unwrap();
        assert_eq!(plan.target.kind, InstallKind::Fresh, "no exe, no service: the case of the finding");
        assert_eq!(serde_json::to_value(&plan.config).unwrap(), serde_json::to_value(&kept).unwrap());
        assert_eq!(plan.watchfolder_source, WatchfolderSource::ExistingConfig);
        assert!(!plan.write_config);
    }

    /// R2: whoever owns the folder can rewrite its rights after the lockdown.
    #[test]
    fn a_folder_another_account_owns_is_refused_before_anything_is_touched() {
        let probe = FakeProbe { owner: Some(r"NEWSROOM\someone".into()), ..new_machine() };
        let log = Log::default();
        let e = plan_install(&InstallOptions { yes: true, ..opts() }, &probe)
            .and_then(|plan| run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).map(|_| ()))
            .unwrap_err();
        assert!(e.downcast_ref::<Refusal>().is_some());
        assert_eq!(
            e.to_string(),
            r#"C:\OmniTest exists and belongs to NEWSROOM\someone, not to Administrators. Setup does not install into a folder another account controls. Remove the folder or choose another one; if it is yours, run: icacls "C:\OmniTest" /setowner Administrators /T"#
        );
        assert!(log.lines().is_empty());
        // An upgrade or an account change meets the same check: it is in `locate`.
        let mut upgrade = installed(DEFAULT_SERVICE_ACCOUNT);
        upgrade.owner = Some(r"NEWSROOM\someone".into());
        let o = InstallOptions { account: Some(r"DOMAIN\svc_test".into()), ..opts() };
        assert!(plan_install(&o, &upgrade).unwrap_err().downcast_ref::<Refusal>().is_some());
    }

    /// R5: until the account is changed in place, a typo must not reach the step
    /// that removes the old registration.
    #[test]
    fn an_account_windows_does_not_know_is_refused_before_anything_is_touched() {
        let mut probe = installed(DEFAULT_SERVICE_ACCOUNT);
        probe.unknown_accounts = vec![r"DOMAIN\svc_tset".into()];
        let o = InstallOptions { account: Some(r"DOMAIN\svc_tset".into()), ..opts() };
        let log = Log::default();
        let e = plan_install(&o, &probe)
            .and_then(|plan| run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).map(|_| ()))
            .unwrap_err();
        assert!(e.downcast_ref::<Refusal>().is_some());
        assert_eq!(e.to_string(), r"Windows does not know the account DOMAIN\svc_tset; check the spelling (DOMAIN\user or .\user).");
        assert!(log.lines().is_empty());
        // Built-in and virtual accounts are never looked up.
        probe.unknown_accounts = vec!["LocalSystem".into(), DEFAULT_SERVICE_ACCOUNT.into(), r"NT AUTHORITY\NetworkService".into()];
        for a in ["LocalSystem", DEFAULT_SERVICE_ACCOUNT, r"NT AUTHORITY\NetworkService"] {
            let o = InstallOptions { account: Some(a.into()), ..opts() };
            assert!(plan_install(&o, &probe).is_ok(), "{a}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_installer_runs_icacls_and_netsh_by_full_path() {
        let system = omni_core::paths::system_tool("kernel32.dll").unwrap();
        for (tool, name) in [(netsh_program().unwrap(), "netsh.exe"), (omni_core::lockdown::icacls().unwrap(), "icacls.exe")] {
            assert!(tool.is_absolute(), "{tool:?}");
            assert_eq!(tool.parent(), system.parent(), "{tool:?}");
            assert_eq!(tool.file_name().unwrap(), name);
        }
    }

    /// S2a: every write and grant would follow a link to the root or above it.
    #[test]
    fn a_root_reached_through_a_link_is_refused_before_anything_is_touched() {
        for link in [r"C:\OmniTest", r"C:\"] {
            let probe = FakeProbe { links: vec![PathBuf::from(link)], ..new_machine() };
            let log = Log::default();
            let e = plan_install(&InstallOptions { yes: true, ..opts() }, &probe)
                .and_then(|plan| run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).map(|_| ()))
                .unwrap_err();
            assert!(e.downcast_ref::<Refusal>().is_some());
            assert_eq!(e.to_string(), format!(r"C:\OmniTest is reached through a link ({link}). Give the real folder with /dir."));
            assert!(log.lines().is_empty());
        }
        // An upgrade too.
        let mut upgrade = installed(DEFAULT_SERVICE_ACCOUNT);
        upgrade.links = vec![PathBuf::from(ROOT)];
        assert!(plan_install(&opts(), &upgrade).unwrap_err().downcast_ref::<Refusal>().is_some());
    }

    /// S2c: a fresh install is refused under a folder someone else may swap; an
    /// upgrade of a running system is told, never blocked.
    #[test]
    fn a_swappable_parent_refuses_a_new_installation_and_warns_an_upgrade() {
        let swappable = Some((PathBuf::from(r"C:\Tools"), r"NT AUTHORITY\Authenticated Users".to_string()));
        let probe = FakeProbe { swappable: swappable.clone(), ..new_machine() };
        let e = plan_install(&InstallOptions { yes: true, ..opts() }, &probe).unwrap_err();
        assert!(e.downcast_ref::<Refusal>().is_some());
        let text = r"C:\Tools can be renamed or re-permissioned by NT AUTHORITY\Authenticated Users, so the service's folder could be swapped. Install directly on a drive (for example D:\OmniIngest) or under Program Files, or remove that permission.";
        assert_eq!(e.to_string(), text);
        let mut upgrade = installed(DEFAULT_SERVICE_ACCOUNT);
        upgrade.swappable = swappable.clone();
        let plan = plan_install(&opts(), &upgrade).unwrap();
        assert_eq!(plan.target.warnings, [format!("! {text}")]);
        assert_eq!(plan.target.header().last().unwrap(), &format!("! {text}"));
        // T5: a service registered elsewhere does not make a new folder an upgrade.
        let elsewhere = FakeProbe {
            swappable,
            service: Some((PathBuf::from(r"D:\OtherOmni\omni-ingest.exe"), DEFAULT_SERVICE_ACCOUNT.into())),
            ..new_machine()
        };
        let e = plan_install(&InstallOptions { yes: true, ..opts() }, &elsewhere).unwrap_err();
        assert_eq!(e.to_string(), text, "no installation in this folder: refused");
    }

    /// T1: a development run is decided by the executable being installed (cargo's
    /// `<repo>\target\…\omni-ingest.exe`, run in place), never by a `Cargo.toml`
    /// near the root, which anyone may write.
    #[test]
    fn only_a_cargo_build_run_in_place_is_exempt_never_a_planted_cargo_toml() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        let release = repo.join("target").join("release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(repo.join("Cargo.toml"), b"[workspace]").unwrap();
        let built = release.join(EXE);
        let is_file = |p: &Path| p.is_file();
        assert!(is_cargo_build(&built, &is_file));
        assert!(!is_cargo_build(&d.path().join("Omni").join(EXE), &is_file));
        // A folder above every root here is swappable: whatever is judged is refused.
        let judged = || Some((d.path().to_path_buf(), "someone".to_string()));
        let owned = |exe: Option<PathBuf>, dir: PathBuf| {
            let mut files = vec![(repo.join("Cargo.toml"), 1)];
            files.extend(exe.iter().map(|e| (e.clone(), 1)));
            FakeProbe { swappable: judged(), files, exe, ..new_machine() }.pipe_locate(dir)
        };
        // The build, in place: exempt (not judged; created plainly; not locked).
        let t = owned(Some(built.clone()), release.clone()).expect("a cargo build in place is not judged");
        assert!(t.development && t.in_place);
        assert_eq!(new_dir_rights(t.development), NewDirRights::Plain);
        // The same exe installed into another root: judged.
        assert!(owned(Some(built.clone()), d.path().join("Omni")).is_err());
        // A planted Cargo.toml above or inside a normal root: still judged, locked and created protected.
        let root = d.path().join("Omni");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(d.path().join("Cargo.toml"), b"").unwrap();
        std::fs::write(root.join("Cargo.toml"), b"").unwrap();
        let planted = |exe: PathBuf| FakeProbe { swappable: judged(), exe: Some(exe), ..new_machine() };
        assert!(planted(PathBuf::from(RELEASE_EXE)).pipe_locate(root.clone()).is_err());
        assert!(planted(root.join(EXE)).pipe_locate(root.clone()).is_err(), "in place, but not a cargo build");
        assert!(is_installed_root(&root.join(EXE), &root), "locked");
        assert_eq!(new_dir_rights(false), NewDirRights::InstallRoot);
    }

    impl FakeProbe {
        fn pipe_locate(self, dir: PathBuf) -> Result<Target> {
            locate(&InstallOptions { dir: Some(dir), yes: true, ..Default::default() }, &self)
        }
    }

    /// S5: a new root (and its missing parents) starts with the P2.16 baseline.
    #[test]
    fn a_new_root_gets_the_baseline_descriptor() {
        assert_eq!(new_dir_rights(false), NewDirRights::InstallRoot);
        assert_eq!(new_dir_rights(true), NewDirRights::Plain);
        assert!(omni_core::lockdown::INSTALL_ROOT_SDDL.contains("(A;OICI;0x1200a9;;;BU)"));
        assert!(!omni_core::lockdown::PROTECTED_SDDL.contains(";;;BU)"), "Setup's work folder stays Administrators+SYSTEM");
    }

    /// T2: the prompts lie between `locate` and the first machine step; what
    /// changed in between is refused before anything is copied.
    #[test]
    fn a_root_or_parent_that_changed_after_planning_is_refused() {
        let fresh = locate(&InstallOptions { yes: true, ..opts() }, &new_machine()).unwrap();
        let root = PathBuf::from(ROOT);
        let parent_root = PathBuf::from(r"C:\");
        let exists = |p: &Path| p == root.as_path() || p == parent_root.as_path();
        let none = |_: &Path| None::<String>;
        let no = |_: &Path| false;
        let unchanged = Recheck { is_dir: &exists, is_link: &no, foreign_owner: &none, swapper: &none, writable: &none };
        assert_eq!(changed_since_plan(&fresh, &unchanged), None);
        let user_owned = |p: &Path| (p == root.as_path()).then(|| r"NEWSROOM\someone".to_string());
        let r = Recheck { foreign_owner: &user_owned, ..unchanged };
        assert_eq!(
            changed_since_plan(&fresh, &r).as_deref(),
            Some(r"C:\OmniTest changed while Setup was running: it now belongs to NEWSROOM\someone; nothing was installed.")
        );
        let link = |p: &Path| p == root.as_path();
        let r = Recheck { is_link: &link, ..unchanged };
        assert!(changed_since_plan(&fresh, &r).unwrap().contains("it is now a link"));
        // U3: a root opened to others meanwhile; an installation already there is not judged on it.
        let open = |p: &Path| (p == root.as_path()).then(|| r"NT AUTHORITY\Authenticated Users".to_string());
        let r = Recheck { writable: &open, ..unchanged };
        assert_eq!(
            changed_since_plan(&fresh, &r).as_deref(),
            Some(r"C:\OmniTest changed while Setup was running: NT AUTHORITY\Authenticated Users can now write to it; nothing was installed.")
        );
        let upgrade = locate(&opts(), &installed(DEFAULT_SERVICE_ACCOUNT)).unwrap();
        assert!(upgrade.installed_here());
        assert_eq!(changed_since_plan(&upgrade, &r), None);
        // A parent below the volume root that became swappable.
        let deep = locate(&InstallOptions { dir: Some(r"C:\Tools\Omni".into()), yes: true, ..Default::default() }, &new_machine()).unwrap();
        let tools = PathBuf::from(r"C:\Tools");
        let exists_tools = |p: &Path| p == tools.as_path() || p == parent_root.as_path();
        let swappable = |p: &Path| (p == tools.as_path()).then(|| "Authenticated Users".to_string());
        let r = Recheck { is_dir: &exists_tools, is_link: &no, foreign_owner: &none, swapper: &swappable, writable: &none };
        assert!(changed_since_plan(&deep, &r).unwrap().starts_with(r"C:\Tools changed while Setup was running"));
        // The volume root itself is never judged for owner or entries.
        let volume = |p: &Path| (p == parent_root.as_path()).then(|| "someone".to_string());
        let r = Recheck { is_dir: &exists_tools, is_link: &no, foreign_owner: &volume, swapper: &volume, writable: &volume };
        assert_eq!(changed_since_plan(&deep, &r), None);
    }

    /// U1: what was missing at the re-check is created by this run or refused,
    /// never adopted.
    #[test]
    fn a_folder_that_appears_after_the_recheck_is_refused_never_adopted() {
        let deep = locate(&InstallOptions { dir: Some(r"C:\Tools\Omni".into()), yes: true, ..Default::default() }, &new_machine()).unwrap();
        let (root, tools, volume) = (PathBuf::from(r"C:\Tools\Omni"), PathBuf::from(r"C:\Tools"), PathBuf::from(r"C:\"));
        let only_volume = |p: &Path| p == volume.as_path();
        let none = |_: &Path| None::<String>;
        let no = |_: &Path| false;
        let r = Recheck { is_dir: &only_volume, is_link: &no, foreign_owner: &none, swapper: &none, writable: &none };
        // Unchanged: both created by us, parent first.
        let made = std::cell::RefCell::new(Vec::new());
        create_install_dir_with(&deep, &r, &mut |d: &Path| {
            made.borrow_mut().push(d.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(*made.borrow(), [tools.clone(), root.clone()]);
        // The root appears between the re-check and its creation.
        let appears = |at: PathBuf| move |d: &Path| {
            if d == at { Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)) } else { Ok(()) }
        };
        let e = create_install_dir_with(&deep, &r, &mut appears(root.clone())).unwrap_err();
        assert!(e.downcast_ref::<Refusal>().is_some());
        assert!(e.to_string().starts_with(r"C:\Tools\Omni changed while Setup was running: it appeared"), "{e}");
        // A missing parent appears (made by someone who may swap it).
        let swaps = |p: &Path| (p == tools.as_path()).then(|| "Authenticated Users".to_string());
        let r2 = Recheck { swapper: &swaps, ..r };
        let e = create_install_dir_with(&deep, &r2, &mut appears(tools.clone())).unwrap_err();
        assert_eq!(
            e.to_string(),
            r"C:\Tools changed while Setup was running: it appeared and Authenticated Users may rename or re-permission it; nothing was installed."
        );
        // An existing root (before the re-check) is used as it is; nothing is created.
        let exists_all = |_: &Path| true;
        let r3 = Recheck { is_dir: &exists_all, ..r };
        let made = std::cell::RefCell::new(0);
        create_install_dir_with(&deep, &r3, &mut |_: &Path| {
            *made.borrow_mut() += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(*made.borrow(), 0);
    }

    /// U3: an existing folder with no installation in it must not be open to others.
    #[test]
    fn an_existing_open_folder_without_an_installation_is_refused() {
        let root = PathBuf::from(ROOT);
        // Empty folder made by an administrator's mkdir: inherits Authenticated Users Modify.
        let weak = FakeProbe { dirs: vec![root.clone()], writable: Some(r"NT AUTHORITY\Authenticated Users".into()), ..new_machine() };
        let log = Log::default();
        let e = plan_install(&InstallOptions { yes: true, ..opts() }, &weak)
            .and_then(|plan| run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).map(|_| ()))
            .unwrap_err();
        assert!(e.downcast_ref::<Refusal>().is_some());
        assert_eq!(
            e.to_string(),
            r"C:\OmniTest already exists and NT AUTHORITY\Authenticated Users can write to it, so files could be planted in it before Setup secures it. Remove the folder and let Setup create it, or choose another folder. Nothing was changed."
        );
        assert!(log.lines().is_empty());
        // A pre-lockdown installation (trusted exe, weak ACL): upgraded as before.
        let mut old = installed(DEFAULT_SERVICE_ACCOUNT);
        old.dirs = vec![root.clone()];
        old.writable = Some(r"NT AUTHORITY\Authenticated Users".into());
        let plan = plan_install(&opts(), &old).unwrap();
        assert!(plan.target.installed_here());
        let log = Log::default();
        run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).unwrap();
        assert!(log.lines().iter().any(|l| l == "#stop_service"), "{:?}", log.lines());
        // A planted exe (untrusted owner) in a weak folder: refused, and no upgrade.
        let mut planted = old;
        planted.service = None;
        planted.configs.clear();
        planted.untrusted = vec![root.join(EXE)];
        let target = locate(&InstallOptions { yes: true, ..opts() }, &FakeProbe { writable: None, ..planted.clone_probe() }).unwrap();
        assert_eq!(target.kind, InstallKind::Fresh, "a planted exe is not an installation");
        let e = plan_install(&InstallOptions { yes: true, ..opts() }, &planted).unwrap_err();
        assert!(e.to_string().contains("can write to it"), "{e}");
        // A locked leftover (no exe, closed ACL) passes.
        let leftover = FakeProbe { dirs: vec![root], ..new_machine() };
        plan_install(&InstallOptions { yes: true, ..opts() }, &leftover).unwrap();
    }

    impl FakeProbe {
        fn clone_probe(&self) -> FakeProbe {
            FakeProbe {
                files: self.files.clone(),
                configs: self.configs.clone(),
                service: self.service.clone(),
                untrusted: self.untrusted.clone(),
                dirs: self.dirs.clone(),
                writable: self.writable.clone(),
                ..new_machine()
            }
        }
    }

    /// U4: the root tree is walked once, the paths outside it once each, and a
    /// cargo build in place not at all.
    #[test]
    fn each_tree_is_walked_once() {
        let o = InstallOptions { watchfolder: Some(r"C:\Dalet\ingest".into()), yes: true, ..opts() };
        let probe = new_machine();
        plan_install(&o, &probe).unwrap();
        assert_eq!(*probe.walks.borrow(), [PathBuf::from(ROOT), PathBuf::from(r"C:\Dalet\ingest")]);
        // A cargo build run in place: the root is never walked.
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        let release = repo.join("target").join("release");
        let probe = FakeProbe { files: vec![(repo.join("Cargo.toml"), 1)], exe: Some(release.join(EXE)), ..new_machine() };
        plan_install(&InstallOptions { dir: Some(release.clone()), yes: true, ..Default::default() }, &probe).unwrap();
        assert!(probe.walks.borrow().iter().all(|w| !w.starts_with(&release)), "{:?}", probe.walks.borrow());
    }

    /// T3/T7: links are refused at plan time, before the service is stopped.
    #[test]
    fn a_link_where_this_run_writes_or_grants_is_refused_before_the_service_is_stopped() {
        let paths = root_paths();
        let refused = |probe: &FakeProbe, o: InstallOptions| {
            let log = Log::default();
            let r = plan_install(&o, probe).and_then(|plan| run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).map(|_| ()));
            (r, log.lines())
        };
        // A junction at archive on a plain upgrade.
        let mut upgrade = installed(DEFAULT_SERVICE_ACCOUNT);
        upgrade.links = vec![paths.archive.clone()];
        let (r, lines) = refused(&upgrade, opts());
        let e = r.unwrap_err();
        assert!(e.downcast_ref::<Refusal>().is_some());
        assert_eq!(
            e.to_string(),
            r"C:\OmniTest\archive is a link inside the installation. Setup does not copy or change permissions through a link. Remove it, or give the real folder in config.json, and run Setup again. Nothing was changed."
        );
        assert!(lines.is_empty(), "the machine was touched: {lines:?}");
        // A link deep in data on a plain upgrade (account kept): nothing walks there.
        upgrade.links = vec![paths.data.join("adblock").join("lists")];
        let (r, lines) = refused(&upgrade, opts());
        r.unwrap();
        assert!(lines.iter().any(|l| l == "#stop_service"));
        // The same link when this run changes permissions.
        let (r, lines) = refused(&upgrade, InstallOptions { account: Some(r"DOMAIN\svc_test".into()), ..opts() });
        assert!(r.unwrap_err().to_string().starts_with(r"C:\OmniTest\data\adblock\lists is a link inside the installation."));
        assert!(lines.is_empty());
        // A link in a local watchfolder outside the root, with an account change.
        let mut probe = installed(DEFAULT_SERVICE_ACCOUNT);
        probe.links = vec![PathBuf::from(r"C:\Dalet\ingest\old")];
        let (r, lines) = refused(&probe, InstallOptions { account: Some(r"DOMAIN\svc_test".into()), ..opts() });
        assert!(r.unwrap_err().to_string().starts_with(r"C:\Dalet\ingest\old is a link inside the installation."));
        assert!(lines.is_empty());
    }

    /// S9: a tool is replaced through a new file and a rename: a hard link at the
    /// destination is cut loose, never written through, and nothing is left behind.
    #[test]
    fn a_tool_is_replaced_by_rename_and_a_hard_link_is_not_written_through() {
        let d = tempfile::tempdir().unwrap();
        let (src, dst, other) = (d.path().join("new.exe"), d.path().join("ffmpeg.exe"), d.path().join("elsewhere.exe"));
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&other, b"someone else's file").unwrap();
        std::fs::hard_link(&other, &dst).unwrap();
        replace_file(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        assert_eq!(std::fs::read(&other).unwrap(), b"someone else's file", "written through a hard link");
        assert!(!staged_name(&dst).exists());
        // A failed copy leaves no staged file and names nothing it did not touch.
        let e = replace_file(&d.path().join("missing.exe"), &dst).unwrap_err();
        assert!(!staged_name(&dst).exists(), "{e}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        // A destination that cannot be replaced (a folder): the staged file is removed.
        let folder = d.path().join("bmxtranswrap.exe");
        std::fs::create_dir(&folder).unwrap();
        assert!(replace_file(&src, &folder).is_err());
        assert!(!staged_name(&folder).exists());
        // Through copy_tools the error names the tool.
        let (from, to) = (d.path().join("from"), d.path().join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(to.join("ffprobe.exe")).unwrap();
        std::fs::write(from.join("ffprobe.exe"), b"probe").unwrap();
        let e = copy_tools(&from, &to, &mut Log::default()).unwrap_err();
        assert!(format!("{e:#}").contains(r"bin\ffprobe.exe"), "{e:#}");
        assert!(!to.join("ffprobe.exe.omni-new").exists());
        // Leftovers are program files.
        assert!(program_files(&to).iter().any(|p| p.ends_with("journalists.seed.json.omni-new")));
    }

    /// S1: nothing is created through a link below the root.
    #[test]
    fn the_state_folders_are_not_created_through_a_link() {
        let d = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(d.path().join("Omni"), "config.json");
        let data = paths.data.clone();
        let e = ensure_dirs_with(&paths, &|p: &Path| p == data).unwrap_err();
        assert!(e.to_string().contains("is reached through a link"), "{e}");
        assert!(!paths.temp.exists(), "nothing was created");
        ensure_dirs_with(&paths, &|_: &Path| false).unwrap();
        assert!(paths.data.join("adblock").is_dir() && paths.temp.join("jobs").is_dir());
    }

    /// The permission changes a fake records, and the links it pretends exist.
    struct FakeAcl {
        log: Log,
        links: Vec<PathBuf>,
        entries: Vec<PathBuf>,
    }

    impl AclOps for FakeAcl {
        fn is_link(&self, path: &Path) -> bool {
            self.links.iter().any(|l| l == path)
        }
        fn first_link_in_tree(&self, path: &Path) -> Option<PathBuf> {
            self.links.iter().find(|l| l.starts_with(path)).cloned()
        }
        fn entries(&self, _dir: &Path) -> Vec<PathBuf> {
            self.entries.clone()
        }
        async fn remove_grants(&mut self, path: &Path, account: &str, recursive: bool) -> Result<()> {
            self.log.push(format!("#remove_grants {} {account} {}", path.display(), if recursive { "/T" } else { "here" }));
            Ok(())
        }
        async fn lock_root(&mut self, root: &Path, _account: Option<&str>) -> Result<()> {
            self.log.push(format!("#lock_root {}", root.display()));
            Ok(())
        }
        async fn grant_modify(&mut self, account: &str, path: &Path, _rights: Rights, _p: &mut dyn Progress) -> bool {
            self.log.push(format!("#grant {account} {}", path.display()));
            true
        }
        async fn lock_dirs(&mut self, dirs: &[PathBuf], _account: omni_core::lockdown::Account) -> Result<()> {
            for d in dirs {
                self.log.push(format!("#lock_dir {}", d.display()));
            }
            Ok(())
        }
    }

    /// S1: the service account may write temp, data, archive...; a link there
    /// must not turn an elevated grant into Modify on something else.
    #[test]
    fn no_permission_change_goes_through_a_link_and_the_rest_still_does() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("OmniTest");
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::with_root(&root, "config.json");
        let mut config = AppConfig::default();
        config.watchfolder_path = r"D:\Dalet".into();
        let account = ServiceAccount { name: DEFAULT_SERVICE_ACCOUNT.into(), password: None };
        let grants = grant_plan_with(&paths, &config, true, &MappedZ);
        let links = vec![paths.temp.clone(), paths.data.join("adblock"), paths.archive.join("2026")];
        let log = Log::default();
        let mut acl = FakeAcl {
            log: log.clone(),
            links: links.clone(),
            entries: ["archive", "bin", "data", "logs", "omni-ingest.exe", "temp"].iter().map(|e| root.join(e)).collect(),
        };
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(apply_acls_on(&paths, &config, Some(&account), Some("LocalSystem"), true, &grants, &mut log.clone(), &mut acl));
        let lines = log.lines();
        let touched = |p: &Path| {
            let p = p.display().to_string();
            lines.iter().any(|l| l.starts_with('#') && (l.ends_with(&p) || l.contains(&format!("{p} "))))
        };
        for bad in [&paths.temp, &paths.data, &paths.archive] {
            assert!(!touched(bad), "{bad:?} was changed through a link: {lines:#?}");
            assert!(lines.iter().any(|l| l.starts_with(&format!("✗ {} contains a link", bad.display()))), "{lines:#?}");
        }
        for good in [&paths.logs, &paths.bin] {
            assert!(lines.iter().any(|l| *l == format!(r"#grant NT SERVICE\OmniIngestService {}", good.display())), "{good:?}: {lines:#?}");
        }
        assert!(lines.contains(&format!(r"#grant NT SERVICE\OmniIngestService {}", paths.config.display())));
        // The root tree holds links: no /T over it, the root alone, each clean entry recursively.
        assert!(lines.contains(&format!(r"#remove_grants {} NT SERVICE\OmniIngestService here", root.display())), "{lines:#?}");
        assert!(lines.contains(&format!(r"#remove_grants {} NT SERVICE\OmniIngestService /T", paths.logs.display())));
        assert!(!lines.contains(&format!(r"#remove_grants {} NT SERVICE\OmniIngestService /T", root.display())));
        assert!(!lines.iter().any(|l| l.starts_with("#lock_root")), "the root's tree holds a link: {lines:#?}");
        assert!(!lines.iter().any(|l| l.starts_with("#lock_dir")), "data and temp both hold links: {lines:#?}");
        assert!(lines.iter().any(|l| l.starts_with("! The service will NOT start")), "{lines:#?}");

        // Without links everything is changed as before.
        let log = Log::default();
        let mut acl = FakeAcl { log: log.clone(), links: Vec::new(), entries: Vec::new() };
        rt.block_on(apply_acls_on(&paths, &config, Some(&account), None, true, &grants, &mut log.clone(), &mut acl));
        let lines = log.lines();
        assert!(lines.contains(&format!("#lock_root {}", root.display())), "{lines:#?}");
        assert!(lines.contains(&format!("#lock_dir {}", paths.temp.display())));
        assert!(lines.contains(&format!(r"#remove_grants {} NT SERVICE\OmniIngestService /T", root.display())));
        assert!(!lines.iter().any(|l| l.contains("contains a link") || l.contains("NOT start")), "{lines:#?}");
    }

    /// R2c: `bin` and `data` are writable by the service account; an elevated
    /// copy must not be sent elsewhere by a link there.
    #[test]
    fn copying_into_a_linked_folder_or_onto_a_linked_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (from, to) = (d.path().join("from"), d.path().join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("ffmpeg.exe"), b"new").unwrap();
        std::fs::write(from.join("journalists.seed.json"), b"[]").unwrap();
        let link = |names: &'static [&'static str]| move |p: &Path| names.iter().any(|n| p.ends_with(n));
        let e = copy_tools_with(&from, &to, &mut Log::default(), &link(&["to"])).unwrap_err();
        assert!(e.to_string().contains("is a link"), "{e}");
        assert!(!to.exists(), "nothing was written");
        let e = copy_tools_with(&from, &to, &mut Log::default(), &link(&["ffmpeg.exe"])).unwrap_err();
        assert!(e.to_string().contains("is a link"), "{e}");
        assert!(!to.join("ffmpeg.exe").exists());
        assert!(copy_missing_with(&from, &to, SEEDS, &mut Log::default(), &link(&["to"])).is_err());
        assert!(!to.join("journalists.seed.json").exists());
        copy_tools_with(&from, &to, &mut Log::default(), &link(&[])).unwrap();
        assert_eq!(std::fs::read(to.join("ffmpeg.exe")).unwrap(), b"new");
    }

    #[cfg(windows)]
    #[test]
    fn copying_into_a_real_directory_symlink_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (from, elsewhere, bin) = (d.path().join("from"), d.path().join("elsewhere"), d.path().join("bin"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(from.join("ffmpeg.exe"), b"new").unwrap();
        if let Err(e) = std::os::windows::fs::symlink_dir(&elsewhere, &bin) {
            eprintln!("SKIPPED copying_into_a_real_directory_symlink_is_refused: this account may not create symbolic links ({e})");
            return;
        }
        assert!(copy_tools(&from, &bin, &mut Log::default()).is_err());
        assert!(!elsewhere.join("ffmpeg.exe").exists());
    }

    /// P13.1b: an answer file's watchfolder on a mapped drive letter is refused with
    /// `watchfolder_problem`'s own text, before the machine is touched.
    #[test]
    fn an_answer_file_watchfolder_on_a_mapped_drive_is_refused_before_anything_is_touched() {
        let mut answered = AppConfig::default();
        answered.watchfolder_path = r"Z:\Dalet\Ingest".into();
        answered.web_port = 8095;
        for yes in [true, false] {
            let o = InstallOptions { yes, ..opts() }.with_base_config(answered.clone());
            let log = Log::default();
            let result = plan_install(&o, &new_machine())
                .and_then(|plan| run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).map(|_| ()));
            let err = result.unwrap_err().to_string();
            let problem = omni_core::paths::watchfolder_problem_with(r"Z:\Dalet\Ingest", &MappedZ).unwrap();
            assert_eq!(err, format!("Cannot install: {problem}"));
            assert!(log.lines().is_empty(), "the machine was touched: {:?}", log.lines());
        }
    }

    #[test]
    fn an_answer_file_is_the_starting_config_of_a_new_installation_only() {
        let mut answered = AppConfig::default();
        answered.watchfolder_path = r"\\dalet\ingest".into();
        answered.web_port = 8095;
        answered.setup_mode = "it".into();
        let o = InstallOptions { account: Some(r"DOMAIN\svc_test".into()), ..opts() }.with_base_config(answered);
        let plan = plan_install(&o, &new_machine()).unwrap();
        assert_eq!(plan.watchfolder_source, WatchfolderSource::Options);
        assert_eq!(plan.config.watchfolder_path, r"\\dalet\ingest");
        assert_eq!((plan.port, plan.config.setup_mode.as_str()), (8095, "it"));
        assert!(plan.write_config && !plan.config.setup_completed);
        // An existing config.json wins over it.
        let mut existing = installed(DEFAULT_SERVICE_ACCOUNT);
        existing.service = None;
        existing.files.clear();
        let mut base = AppConfig::default();
        base.web_port = 8095;
        let plan = plan_install(&InstallOptions { base_config: Some(base), ..opts() }, &existing).unwrap();
        assert_eq!(plan.port, 8090);
    }

    #[test]
    fn program_files_are_what_the_installer_copies_and_never_the_state() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Omni");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin").join("avcodec-61.dll"), b"x").unwrap();
        std::fs::write(root.join("bin").join("notes.txt"), b"x").unwrap();
        let files = program_files(&root);
        for f in ["omni-ingest.exe", "omni-ingest.exe.prev", "omni-ingest.exe.new"] {
            assert!(files.contains(&root.join(f)), "{f}");
        }
        for t in TOOLS.iter().chain(SELF_UPDATING).chain(&["avcodec-61.dll"]) {
            assert!(files.contains(&root.join("bin").join(t)), "{t}");
        }
        // The seeds are the station's data once read: never a program file (R4).
        for s in SEEDS {
            assert!(!files.contains(&root.join("data").join(s)), "{s}");
        }
        assert!(!files.contains(&root.join("bin").join("notes.txt")));
        for state in ["config.json", "data", "logs", "temp", "archive", "watchfolder", "bin"] {
            assert!(!files.contains(&root.join(state)), "{state}");
        }
        assert!(files.iter().all(|f| f.starts_with(&root)));
    }

    #[test]
    fn without_administrator_rights_nothing_is_planned() {
        let probe = FakeProbe { admin: false, ..new_machine() };
        let err = plan_install(&InstallOptions { yes: true, ..opts() }, &probe).unwrap_err().to_string();
        assert!(err.starts_with("The installer needs administrator rights."), "{err}");
    }

    #[test]
    fn an_upgrade_keeps_the_service_account_and_the_config_and_asks_nothing() {
        let probe = installed(DEFAULT_SERVICE_ACCOUNT);
        let plan = plan_install(&opts(), &probe).unwrap();
        assert_eq!(plan.target.kind, InstallKind::Upgrade { had_previous_exe: true });
        assert!(plan.questions.is_empty(), "{:?}", plan.questions);
        assert_eq!(plan.account, AccountDecision::Keep { current: Some(DEFAULT_SERVICE_ACCOUNT.into()) });
        assert!(!plan.account.registers() && !plan.switch_hint);
        assert!(plan.grants.is_empty(), "no registration, no ACL work");
        assert!(!plan.write_config, "an existing config.json is not rewritten");
        assert_eq!(plan.watchfolder_source, WatchfolderSource::ExistingConfig);
        assert_eq!(plan.config.watchfolder_path, r"C:\Dalet\ingest");
        assert_eq!(plan.port, 8090);
        assert_eq!(plan.target.header()[2], "  Upgrading the existing installation.");
        // A kept LocalSystem service gets the hint.
        let plan = plan_install(&opts(), &installed("LocalSystem")).unwrap();
        assert!(!plan.account.registers() && plan.switch_hint);
    }

    /// The SCM reports the service's whole command line. Read as a path it never
    /// matched the install folder's exe, so every upgrade re-registered the service
    /// (and moved a named or LocalSystem service to the default account).
    #[test]
    fn an_in_place_upgrade_of_a_service_registered_with_its_arguments_keeps_its_account() {
        let raw = r"C:\OmniTest\omni-ingest.exe run-service";
        let target = root_paths().root.join(EXE);
        let same = |a: &Path, b: &Path| key(a) == key(b);
        // What was wrong: the raw command line re-registers.
        let wrong = account_plan_with(Some(Path::new(raw)), &target, None, Some(r"DOMAIN\svc_test"), &same);
        assert!(wrong.register, "control: the raw command line never matches");
        // The path the service now reports.
        let mut probe = installed(r"DOMAIN\svc_test");
        probe.service = Some((omni_service::exe_of_service_command(raw), r"DOMAIN\svc_test".into()));
        let plan = plan_install(&opts(), &probe).unwrap();
        assert_eq!(plan.account, AccountDecision::Keep { current: Some(r"DOMAIN\svc_test".into()) });
        assert!(plan.questions.is_empty(), "no password is asked for a kept account");
        assert!(plan.grants.is_empty());
        assert_eq!(plan.target.kind, InstallKind::Upgrade { had_previous_exe: true });
    }

    #[test]
    fn an_aborted_prompt_is_a_cancel_and_other_prompt_failures_are_not() {
        use inquire::InquireError;
        for e in [InquireError::OperationCanceled, InquireError::OperationInterrupted] {
            let err = answered::<String>(Err(e), "No password given").unwrap_err();
            assert!(err.downcast_ref::<Cancelled>().is_some());
            assert_eq!(err.to_string(), "Installation cancelled.");
        }
        let err = answered::<String>(Err(InquireError::NotTTY), "No password given").unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_none());
        assert_eq!(err.to_string(), "No password given");
    }

    #[test]
    fn an_upgrade_whose_service_points_at_another_folder_registers_it_again_here() {
        let mut probe = installed(DEFAULT_SERVICE_ACCOUNT);
        probe.files.clear();
        probe.service = Some((PathBuf::from(r"D:\OldOmni\omni-ingest.exe"), "LocalSystem".into()));
        let plan = plan_install(&opts(), &probe).unwrap();
        assert_eq!(plan.target.kind, InstallKind::Upgrade { had_previous_exe: false });
        assert!(plan.account.registers());
        assert_eq!(plan.account, AccountDecision::Passwordless(DEFAULT_SERVICE_ACCOUNT.into()));
        assert!(!plan.switch_hint);
        assert_eq!(plan.previous_account.as_deref(), Some("LocalSystem"), "its old grants are stripped");
        // The old registration is stopped and removed before the new one.
        let log = Log::default();
        run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).unwrap();
        let steps: Vec<String> = log.lines().into_iter().filter(|l| l.starts_with('#')).collect();
        let at = |s: &str| steps.iter().position(|l| l.starts_with(s)).unwrap_or_else(|| panic!("no {s}: {steps:?}"));
        assert!(at("#stop_service") < at("#copy_executable"));
        assert!(at("#uninstall_service") < at("#install_service"));
    }

    /// An old installation at E:\OldOmni with a relative watchfolder and its database.
    fn with_old_install(wal_len: Option<u64>) -> (FakeProbe, PathBuf) {
        let old = PathBuf::from(r"E:\OldOmni");
        let old_paths = AppPaths::with_root(&old, "config.json");
        let mut c = AppConfig::default();
        c.watchfolder_path = "watchfolder".into();
        let db = old_paths.resolve(&c.database_path);
        let mut probe = new_machine();
        probe.files.push((db.clone(), 4096));
        if let Some(n) = wal_len {
            probe.files.push((db.with_extension("db-wal"), n));
        }
        probe.configs.push((old_paths.config.clone(), c));
        (probe, old)
    }

    #[test]
    fn an_import_from_a_stopped_installation_brings_its_config_and_keeps_its_watchfolder() {
        let (probe, old) = with_old_install(Some(0));
        let o = InstallOptions { import: Some(old.clone()), ..opts() };
        let plan = plan_install(&o, &probe).unwrap();
        assert!(plan.questions.is_empty(), "the imported watchfolder is not asked for");
        assert_eq!(plan.import.as_deref(), Some(old.as_path()));
        assert_eq!(plan.watchfolder_source, WatchfolderSource::Import);
        assert_eq!(PathBuf::from(&plan.config.watchfolder_path), old.join("watchfolder"));
        assert_eq!(plan.config.database_path, "data/omni.db");
        assert!(plan.write_config);
        // The copy is the first machine step after the folder exists.
        let log = Log::default();
        run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).unwrap();
        assert_eq!(log.lines()[..2], [format!("#create_dir {ROOT}"), r"#import E:\OldOmni".to_string()]);
    }

    #[test]
    fn an_import_from_a_database_still_in_use_is_refused_before_anything_is_copied() {
        let (probe, old) = with_old_install(Some(8192));
        let o = InstallOptions { import: Some(old), ..opts() };
        let err = plan_install(&o, &probe).unwrap_err().to_string();
        assert!(err.contains("is still in use (its write-ahead log is not empty)"), "{err}");
        // Nor into an installation that has a config of its own.
        let (mut probe, old) = with_old_install(None);
        probe.configs.push((root_paths().config, AppConfig::default()));
        let err = plan_install(&InstallOptions { import: Some(old), ..opts() }, &probe).unwrap_err().to_string();
        assert!(err.contains("import only into a new installation"), "{err}");
    }

    #[test]
    fn no_firewall_and_a_custom_port_are_planned_as_given() {
        let o = InstallOptions { yes: true, no_firewall: true, ..opts() };
        assert_eq!(plan_install(&o, &new_machine()).unwrap().firewall, None);
        let o = InstallOptions { yes: true, port: Some(8091), ..opts() };
        let plan = plan_install(&o, &new_machine()).unwrap();
        assert_eq!(plan.port, 8091);
        assert_eq!(plan.firewall, Some(8091));
        assert_eq!(plan.health.port(), 8091);
        // A port on an upgrade rewrites config.json.
        let plan = plan_install(&InstallOptions { port: Some(8092), ..opts() }, &installed(DEFAULT_SERVICE_ACCOUNT)).unwrap();
        assert!(plan.write_config);
        assert_eq!(plan.config.web_port, 8092);
    }

    #[test]
    fn the_plan_s_grants_are_grant_plan_s() {
        let cases = [
            InstallOptions { yes: true, ..opts() },
            InstallOptions { yes: true, account: Some("LocalSystem".into()), ..opts() },
            InstallOptions { watchfolder: Some(r"\\dalet\ingest".into()), account: Some(r"DOMAIN\svc_test".into()), ..opts() },
            InstallOptions { watchfolder: Some(r"C:\Dalet\ingest".into()), ..opts() },
            InstallOptions { watchfolder: Some(r"C:\OmniTest\drop".into()), account: Some("LocalSystem".into()), ..opts() },
        ];
        for o in cases {
            let plan = plan_install(&o, &new_machine()).unwrap();
            let named = !matches!(plan.account, AccountDecision::LocalSystem);
            assert!(!plan.grants.is_empty());
            assert_eq!(plan.grants, grant_plan(&plan.target.paths, &plan.config, named), "{o:?}");
        }
    }

    #[test]
    fn the_password_is_never_in_debug_output() {
        let o = InstallOptions {
            watchfolder: Some(r"\\dalet\ingest".into()),
            account: Some(r"DOMAIN\svc_test".into()),
            ..opts()
        };
        let plan = plan_install(&o, &new_machine()).unwrap();
        let answers = Answers { password: Some(PASSWORD.into()), ..Default::default() };
        let log = Log::default();
        let report = run(&plan, &answers, &log, FakeMachine::new(&log)).unwrap();
        // It does reach the registration…
        assert!(log.lines().iter().any(|l| l == r"#install_service C:\OmniTest\omni-ingest.exe as DOMAIN\svc_test password=given"));
        // …and nothing a front end might print.
        for shown in [format!("{answers:?}"), format!("{plan:?}"), format!("{report:?}"), log.lines().join("\n")] {
            assert!(!shown.contains(PASSWORD), "{shown}");
        }
        assert!(format!("{answers:?}").contains("<hidden>"));
        // No password, no install.
        let log = Log::default();
        let err = run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).unwrap_err();
        assert_eq!(err.to_string(), "No password given");
        assert!(log.lines().is_empty());
    }

    // ---- the execution, on a fake machine ------------------------------------------

    struct FakeMachine {
        log: Log,
        had_previous: bool,
        health: Option<String>,
    }

    impl FakeMachine {
        fn new(log: &Log) -> Self {
            FakeMachine { log: log.clone(), had_previous: false, health: Some(HEALTHY.into()) }
        }
    }

    const HEALTHY: &str = r#"{"status":"ok","checks":{"database":"ok","watchfolder":"warn"}}"#;

    impl Machine for FakeMachine {
        fn create_dir(&mut self, t: &Target) -> Result<()> {
            self.log.push(format!("#create_dir {}", t.dir.display()));
            Ok(())
        }
        fn import(&mut self, old: &Path, _new: &AppPaths, _p: &mut dyn Progress) -> Result<()> {
            self.log.push(format!("#import {}", old.display()));
            Ok(())
        }
        fn stop_service(&mut self) -> Result<()> {
            self.log.push("#stop_service");
            Ok(())
        }
        fn copy_executable(&mut self, source: &Path, target: &Path, _p: &mut dyn Progress) -> Result<bool> {
            self.log.push(format!("#copy_executable {} -> {}", source.display(), target.display()));
            Ok(self.had_previous)
        }
        fn copy_release(&mut self, source_dir: &Path, _paths: &AppPaths, _p: &mut dyn Progress) -> Result<()> {
            self.log.push(format!("#copy_release {}", source_dir.display()));
            Ok(())
        }
        fn ensure_dirs(&mut self, _paths: &AppPaths) -> Result<()> {
            self.log.push("#ensure_dirs");
            Ok(())
        }
        fn create_watchfolder(&mut self, dir: &Path) {
            self.log.push(format!("#create_watchfolder {}", dir.display()));
        }
        fn save_config(&mut self, _config: &AppConfig, path: &Path) -> Result<()> {
            self.log.push(format!("#save_config {}", path.display()));
            Ok(())
        }
        fn uninstall_service(&mut self) -> Result<()> {
            self.log.push("#uninstall_service");
            Ok(())
        }
        fn install_service(&mut self, exe: &Path, account: Option<&ServiceAccount>) -> Result<()> {
            let pw = if account.is_some_and(|a| a.password.as_deref() == Some(PASSWORD)) { "given" } else { "none" };
            self.log.push(format!("#install_service {} as {} password={pw}", exe.display(), account_label(account)));
            Ok(())
        }
        async fn apply_acls(&mut self, plan: &InstallPlan, _a: Option<&ServiceAccount>, _p: &mut dyn Progress) {
            self.log.push(format!("#apply_acls {} grants", plan.grants.len()));
        }
        async fn open_firewall(&mut self, exe: &Path, port: u16, _p: &mut dyn Progress) {
            self.log.push(format!("#open_firewall {} {port}", exe.display()));
        }
        fn start_service(&mut self) -> Result<()> {
            self.log.push("#start_service");
            Ok(())
        }
        fn wait_for_health(&mut self, addr: SocketAddr, tls: bool, timeout: Duration) -> Option<String> {
            self.log.push(format!("#wait_for_health {addr} tls={tls} {}s", timeout.as_secs()));
            self.health.clone()
        }
        fn restore_previous(&mut self, target_exe: &Path) -> Result<()> {
            self.log.push(format!("#restore_previous {}", target_exe.display()));
            Ok(())
        }
    }

    fn run(plan: &InstallPlan, answers: &Answers, log: &Log, mut machine: FakeMachine) -> Result<InstallReport> {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(execute_with(plan, answers, &mut log.clone(), &mut machine))
    }

    fn host() -> String {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this-pc".into())
    }

    /// The lines and machine steps of the pre-split `install()` for a fresh
    /// install with defaults, in its order.
    #[test]
    fn a_fresh_install_says_and_does_what_it_did_before_in_the_same_order() {
        let plan = plan_install(&InstallOptions { yes: true, ..opts() }, &new_machine()).unwrap();
        let log = Log::default();
        let report = run(&plan, &Answers::default(), &log, FakeMachine::new(&log)).unwrap();
        let mut expected: Vec<String> = [
            r"#create_dir C:\OmniTest",
            r"#copy_executable D:\Release\omni-ingest.exe -> C:\OmniTest\omni-ingest.exe",
            r"#copy_release D:\Release",
            "#ensure_dirs",
            r"#create_watchfolder C:\OmniTest\watchfolder",
            r"#save_config C:\OmniTest\config.json",
            r"  Configuration written to C:\OmniTest\config.json",
            r"#install_service C:\OmniTest\omni-ingest.exe as NT SERVICE\OmniIngestService password=none",
            r"  Service registered: automatic (delayed) start, restarts after a failure, runs as NT SERVICE\OmniIngestService.",
        ]
        .map(String::from)
        .to_vec();
        expected.push(format!("#apply_acls {} grants", plan.grants.len()));
        expected.push(r"#open_firewall C:\OmniTest\omni-ingest.exe 8080".into());
        expected.extend(
            [
                "  Starting the service and waiting for it to answer (first start can take a few minutes)…",
                "#start_service",
                "#wait_for_health 127.0.0.1:8080 tls=false 240s",
                "",
                "✓ OmniDownloader is running.",
                "  Health: ok",
                "    ✓ database: ok",
                "    ! watchfolder: warn",
            ]
            .map(String::from),
        );
        expected.extend(closing_lines(&plan, &host()));
        assert_eq!(log.lines(), expected);
        assert_eq!(report.kind, InstallKind::Fresh);
        assert!(!report.rolled_back);
        assert_eq!(report.admin_url, "http://127.0.0.1:8080/admin");
        assert_eq!(report.health, ["  Health: ok", "    ✓ database: ok", "    ! watchfolder: warn"]);
    }

    #[test]
    fn an_upgrade_says_and_does_what_it_did_before_in_the_same_order() {
        let plan = plan_install(&opts(), &installed("LocalSystem")).unwrap();
        let log = Log::default();
        let mut machine = FakeMachine::new(&log);
        machine.had_previous = true;
        run(&plan, &Answers::default(), &log, machine).unwrap();
        let mut expected: Vec<String> = [
            r"#create_dir C:\OmniTest",
            r#"  The service runs as LocalSystem (kept). To switch: Install.cmd --account "NT SERVICE\OmniIngestService""#,
            "  Stopping the service…",
            "#stop_service",
            r"#copy_executable D:\Release\omni-ingest.exe -> C:\OmniTest\omni-ingest.exe",
            r"#copy_release D:\Release",
            "#ensure_dirs",
            r"#create_watchfolder C:\Dalet\ingest",
            r"#open_firewall C:\OmniTest\omni-ingest.exe 8090",
            "  Starting the service and waiting for it to answer (first start can take a few minutes)…",
            "#start_service",
            "#wait_for_health 127.0.0.1:8090 tls=false 240s",
            "",
            "✓ OmniDownloader is running.",
            "  Health: ok",
            "    ✓ database: ok",
            "    ! watchfolder: warn",
        ]
        .map(String::from)
        .to_vec();
        expected.extend(closing_lines(&plan, &host()));
        assert_eq!(log.lines(), expected);
        assert_eq!(
            log.lines().last().unwrap(),
            r"  Previous version kept as C:\OmniTest\omni-ingest.exe.prev."
        );
    }

    #[test]
    fn an_upgrade_that_does_not_answer_puts_the_previous_version_back() {
        let plan = plan_install(&opts(), &installed(DEFAULT_SERVICE_ACCOUNT)).unwrap();
        let log = Log::default();
        let mut machine = FakeMachine::new(&log);
        machine.had_previous = true;
        machine.health = None;
        let report = run(&plan, &Answers::default(), &log, machine).unwrap();
        assert!(report.rolled_back);
        assert_eq!(report.logs, root_paths().logs);
        let lines = log.lines();
        let tail: Vec<&str> = lines[lines.len() - 6..].iter().map(String::as_str).collect();
        assert_eq!(
            tail,
            [
                "#wait_for_health 127.0.0.1:8090 tls=false 240s",
                "",
                "✗ The new version did not answer within 240 s. Putting the previous one back…",
                "#stop_service",
                r"#restore_previous C:\OmniTest\omni-ingest.exe",
                "#start_service",
            ]
        );
    }

    #[test]
    fn a_service_that_does_not_answer_without_a_previous_version_is_an_error_not_a_rollback() {
        let plan = plan_install(&InstallOptions { yes: true, ..opts() }, &new_machine()).unwrap();
        let log = Log::default();
        let mut machine = FakeMachine::new(&log);
        machine.health = None;
        let err = run(&plan, &Answers::default(), &log, machine).unwrap_err().to_string();
        assert!(err.starts_with("The service did not answer on 127.0.0.1:8080 within 240 s."), "{err}");
        assert!(!log.lines().iter().any(|l| l.starts_with("#restore_previous")));
        assert_eq!(log.lines().last().unwrap(), "#wait_for_health 127.0.0.1:8080 tls=false 240s");
    }

    #[test]
    fn a_new_installation_is_sent_to_the_setup_wizard_on_this_computer() {
        let plan = plan_install(&InstallOptions { yes: true, port: Some(8091), ..opts() }, &new_machine()).unwrap();
        let text = closing_lines(&plan, "NEWSROOM-PC").join("\n");
        assert!(text.contains("  Admin:     http://NEWSROOM-PC:8091/admin"), "{text}");
        // Each sentence whole, however it is wrapped, and in this order.
        let flat = closing_lines(&plan, "NEWSROOM-PC").iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
        let sentences = [
            "Next: on THIS computer, open http://127.0.0.1:8091/admin and sign in as Admin / Admin, \
             the first-run account, which works only from this computer.",
            "The setup wizard then opens by itself; it replaces that account with your own administrator \
             and walks you through the watchfolder and the mailbox.",
            "To let the MCR desks open the panel without signing in, choose Intermediate in the wizard \
             and enter the newsroom network in its Network step (you can run the wizard again later \
             from the admin page).",
            "A watchfolder on a NAS or an IP-addressed share needs the service to run under an account; \
             the wizard tells you what to do.",
        ];
        let mut from = 0;
        for s in sentences {
            let at = flat[from..].find(s).unwrap_or_else(|| panic!("missing or out of order: {s:?}\n{text}"));
            from += at + s.len();
        }
        assert!(!text.contains("LLM"), "{text}");
        assert!(!text.contains("Nothing else needs a terminal"), "{text}");
        // The pre-wizard instructions are gone.
        for old in ["deactivate", "Users: create"] {
            assert!(!text.contains(old), "still says {old:?}: {text}");
        }
        let upgrade = plan_install(&opts(), &installed(DEFAULT_SERVICE_ACCOUNT)).unwrap();
        assert!(!closing_lines(&upgrade, "NEWSROOM-PC").join("\n").contains("setup wizard"));
    }

    #[test]
    fn the_health_summary_is_what_the_console_printed() {
        assert_eq!(health_lines(HEALTHY), ["  Health: ok", "    ✓ database: ok", "    ! watchfolder: warn"]);
        assert!(health_lines("").is_empty(), "TLS: an accepted connection, no body");
    }
}
