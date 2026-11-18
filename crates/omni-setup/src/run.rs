//! What Setup does once its command line is parsed: elevate (or ask the
//! unelevated parent to), then install, change the account or uninstall by
//! driving `omni_cli::install`, and end with one exit code.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use omni_cli::install::{
    execute, install_console, plan_answered, plan_install, remove_firewall_rule, remove_service_registration,
    rolled_back_message, same_dir, Answers, Cancelled, InstallKind, InstallOptions, InstallReport, Probe, Question, Refusal,
    SystemProbe,
};
use omni_core::config::AppConfig;
use omni_core::paths::{AppPaths, DriveLookup};

use crate::args::{command_line, relaunch_args, Mode};
use crate::elevate::{self, Relaunch};
use crate::exit::{admin_url, coded, decode_child_exit, encode_open, exit_code_for, fail, ChildResult, ExitCode};
use crate::log::{append_setup_log, now_unix, yyyymmdd, SetupLog};
use crate::payload::{self, open_setup, PayloadError};
use crate::registry::{self, SETUP_EXE};
use crate::removal::{execute_removals, removal_plan, Removal};
use crate::workdir::create_secure_work_dir;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How a run ended: the process exit code, and the error to show (always,
/// `/quiet` included).
#[derive(Debug)]
pub struct Finished {
    pub code: u32,
    pub error: Option<String>,
}

/// What a successful mode asks of the end of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Done {
    Ok,
    /// A new installation: open its admin page.
    OpenAdmin { tls: bool, port: u16 },
}

/// The probe of the machine with another executable as "the one being installed".
pub struct ExeProbe {
    pub exe: PathBuf,
}

impl Probe for ExeProbe {
    fn can_install_services(&self) -> bool {
        SystemProbe.can_install_services()
    }
    fn current_exe(&self) -> std::io::Result<PathBuf> {
        Ok(self.exe.clone())
    }
    fn installed_exe(&self) -> Option<PathBuf> {
        SystemProbe.installed_exe()
    }
    fn installed_account(&self) -> Option<String> {
        SystemProbe.installed_account()
    }
    fn exists(&self, path: &Path) -> bool {
        SystemProbe.exists(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        SystemProbe.is_file(path)
    }
    fn file_len(&self, path: &Path) -> Option<u64> {
        SystemProbe.file_len(path)
    }
    fn load_config(&self, path: &Path) -> Result<AppConfig> {
        SystemProbe.load_config(path)
    }
    fn same_dir(&self, a: &Path, b: &Path) -> bool {
        SystemProbe.same_dir(a, b)
    }
    fn drives(&self) -> &dyn DriveLookup {
        &omni_core::paths::SystemDrives
    }
    fn foreign_owner(&self, dir: &Path) -> Option<String> {
        SystemProbe.foreign_owner(dir)
    }
    fn account_exists(&self, name: &str) -> bool {
        SystemProbe.account_exists(name)
    }
    fn is_link(&self, path: &Path) -> bool {
        SystemProbe.is_link(path)
    }
    fn swappable_ancestor(&self, root: &Path) -> Option<(PathBuf, String)> {
        SystemProbe.swappable_ancestor(root)
    }
    fn first_link_under(&self, dir: &Path) -> Option<PathBuf> {
        SystemProbe.first_link_under(dir)
    }
    fn untrusted_owner(&self, path: &Path) -> Option<String> {
        SystemProbe.untrusted_owner(path)
    }
    fn writable_by(&self, dir: &Path) -> Option<String> {
        SystemProbe.writable_by(dir)
    }
}

/// `scheme://host:port/...` → (tls, port).
pub fn parse_admin_url(url: &str) -> Option<(bool, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    let host_port = rest.split('/').next()?;
    let port = host_port.rsplit_once(':')?.1.parse().ok()?;
    Some((tls, port))
}

/// The line for plan questions `/quiet` cannot answer; `None` when there are none.
pub fn unanswerable(questions: &[Question]) -> Option<String> {
    questions.iter().find_map(|q| match q {
        Question::NeedPassword { account } => Some(format!(
            "{account} needs a password, which an unattended install is never given. Run Setup without /quiet \
             to type it at the prompt, or use the virtual account (/account default)."
        )),
        Question::ConfirmUncWithoutAccount { prompt, .. } => {
            Some(format!("Setup would have to ask \"{prompt}\"; an unattended install cannot. Give /account."))
        }
        Question::NeedWatchfolder { .. } => Some("Setup would have to ask for the watchfolder; give it in an answer file.".into()),
    })
}

/// A planning error: nothing on the machine was changed. A refusal's notice is shown first.
fn refused(e: anyhow::Error, log: &mut SetupLog) -> anyhow::Error {
    if let Some(r) = e.downcast_ref::<Refusal>() {
        log.say("");
        for l in &r.notice {
            log.say(l);
        }
    }
    coded(ExitCode::Refused, e)
}

/// Plan, refuse what `/quiet` cannot answer, execute.
async fn quiet_install(o: &InstallOptions, probe: &dyn Probe, log: &mut SetupLog) -> Result<InstallReport> {
    let plan = plan_answered(o, &Answers::default(), probe).map_err(|e| refused(e, log))?;
    for l in plan.target.header() {
        log.say(&l);
    }
    if let Some(why) = unanswerable(&plan.questions) {
        return Err(fail(ExitCode::Refused, why));
    }
    execute(&plan, &Answers::default(), log).await
}

/// The installer, quiet or at the console, with the plan checked first (read-only)
/// so a refusal exits as one.
async fn install_with(o: &InstallOptions, probe: &dyn Probe, quiet: bool, log: &mut SetupLog) -> Result<InstallReport> {
    if quiet {
        return quiet_install(o, probe, log).await;
    }
    plan_install(o, probe).map_err(|e| refused(e, log))?;
    install_console(o, probe, log).await
}

/// A rolled-back run as an error: exit 7 when the previous state is back and
/// running, 1 when the way back itself did not get there (`account_change`:
/// `/change-account`'s wording).
pub fn rolled_back(report: &InstallReport, account_change: bool) -> anyhow::Error {
    let code = if report.restored_running { ExitCode::RolledBack } else { ExitCode::Failed };
    fail(code, rolled_back_message(report, account_change))
}

/// Replace `<dir>\OmniIngest-Setup.exe` with `write`'s bytes, through a temporary name.
fn replace_setup_exe(dir: &Path, write: &mut dyn FnMut(&mut std::fs::File) -> std::result::Result<u64, PayloadError>) -> Result<()> {
    let target = dir.join(SETUP_EXE);
    let staged = dir.join(format!("{SETUP_EXE}.new"));
    let result = (|| -> Result<()> {
        let mut f = std::fs::File::create(&staged)?;
        write(&mut f)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&staged, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

fn write_apps_entry(dir: &Path, log: &mut SetupLog) {
    let values = registry::uninstall_values(dir, VERSION, &yyyymmdd(now_unix()));
    if let Err(e) = registry::write_entry(&values) {
        log.say(&format!("! The Apps & Features entry was not written ({e:#}); uninstall with {SETUP_EXE} /uninstall."));
    }
}

async fn install(
    exe: &Path,
    dir: &Path,
    account: Option<String>,
    no_firewall: bool,
    answer: Option<&Path>,
    quiet: bool,
    log: &mut SetupLog,
) -> Result<Done> {
    let answered = match answer {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("Cannot read the answer file {}", path.display()))
                .map_err(|e| coded(ExitCode::Refused, e))?;
            let doc: serde_json::Value = serde_json::from_str(&text)
                .with_context(|| format!("The answer file {} is not JSON", path.display()))
                .map_err(|e| coded(ExitCode::Refused, e))?;
            Some(omni_core::answer_file::apply(&doc, AppConfig::default())?)
        }
        None => None,
    };
    let mut verified = open_setup(exe)?.verify()?;
    let mut work = create_secure_work_dir().context("Cannot create Setup's work folder").map_err(|e| coded(ExitCode::Failed, e))?;
    let result = async {
        verified.extract(work.path())?;
        let release_exe = work.path().join("omni-ingest.exe");
        if !release_exe.is_file() {
            return Err(PayloadError::Archive("it holds no omni-ingest.exe".into()).into());
        }
        let probe = ExeProbe { exe: release_exe };
        let mut o = InstallOptions { dir: Some(dir.to_path_buf()), account, yes: quiet, no_firewall, ..Default::default() };
        if let Some(applied) = answered {
            // The rule is "no config.json yet", not "nothing installed": an
            // uninstall that kept the data leaves one behind (R7).
            if probe.exists(&AppPaths::with_root(dir, "config.json").config) {
                log.say(
                    "  The answer file is used only for a new installation (no config.json yet); the existing configuration is kept.",
                );
            } else {
                for note in &applied.notes {
                    log.say(&format!("  {note}"));
                }
                o = o.with_base_config(applied.config);
            }
        }
        let report = install_with(&o, &probe, quiet, log).await?;
        if report.rolled_back {
            return Err(rolled_back(&report, false));
        }
        if let Err(e) = replace_setup_exe(dir, &mut |f| verified.write_stub(f)) {
            log.say(&format!("! {SETUP_EXE} was not copied to {} ({e:#}); keep this Setup to uninstall later.", dir.display()));
        }
        write_apps_entry(dir, log);
        Ok(match (report.kind, parse_admin_url(&report.admin_url)) {
            (InstallKind::Fresh, Some((tls, port))) => Done::OpenAdmin { tls, port },
            _ => Done::Ok,
        })
    }
    .await;
    if let Some(line) = work.remove() {
        log.say(&line);
    }
    result
}

/// The account `/change-account` was given, or asked for at the console:
/// empty means the virtual account; no answer at all cancels.
pub fn account_to_use(given: Option<&str>, console: &mut dyn Console) -> Result<String> {
    if let Some(a) = given {
        return Ok(a.to_string());
    }
    let answer = console
        .ask("Account the service should run as (empty = the built-in service account): ")
        .ok_or(Cancelled)?;
    let answer = answer.trim();
    Ok(if answer.is_empty() || answer.eq_ignore_ascii_case("default") {
        omni_service::DEFAULT_SERVICE_ACCOUNT.to_string()
    } else {
        answer.to_string()
    })
}

async fn change_account(
    account: Option<&str>,
    no_firewall: bool,
    quiet: bool,
    running: &Path,
    console: &mut dyn Console,
    log: &mut SetupLog,
) -> Result<(PathBuf, Done)> {
    let Some(exe) = omni_service::installed_service_exe() else {
        return Err(fail(ExitCode::Refused, "The OmniIngest service is not installed on this computer; there is no account to change."));
    };
    let account = account_to_use(account, console)?;
    let dir = exe.parent().context("The installed service has no folder").map_err(|e| coded(ExitCode::Refused, e))?.to_path_buf();
    if !exe.is_file() {
        return Err(fail(ExitCode::Refused, format!("The service points at {}, which does not exist.", exe.display())));
    }
    let probe = ExeProbe { exe };
    let o = InstallOptions { dir: Some(dir.clone()), account: Some(account), yes: quiet, no_firewall, ..Default::default() };
    let report = install_with(&o, &probe, quiet, log).await?;
    if report.rolled_back {
        return Err(rolled_back(&report, true));
    }
    // An installation made by `omni-ingest install` has no Setup in it yet; the
    // Apps & Features entry would point at nothing (R11).
    let stub = dir.join(SETUP_EXE);
    if !same_dir(&stub, running) {
        let copied = open_setup(running)
            .map_err(anyhow::Error::from)
            .and_then(|mut me| replace_setup_exe(&dir, &mut |f| me.write_stub(f)));
        if let Err(e) = copied {
            log.say(&format!("! {SETUP_EXE} was not copied to {} ({e:#}).", dir.display()));
        }
    }
    if stub.is_file() {
        write_apps_entry(&dir, log);
    } else {
        log.say(&format!("  No {SETUP_EXE} in {}: the Apps & Features entry was not written.", dir.display()));
    }
    Ok((dir, Done::Ok))
}

/// Two folders are the same: canonically when both exist, by name (case-folded,
/// trailing separators ignored) when one is gone (a service whose folder was deleted).
pub fn same_folder(a: &Path, b: &Path) -> bool {
    let name = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    same_dir(a, b) || name(a) == name(b)
}

/// The folder `/uninstall` works on, or `None` (refused: nothing to guess).
/// `/dir` as given; else this Setup's own folder only when it is the
/// registered service's or the Apps & Features entry's (a Setup lying beside
/// an unpacked release is not an installation); else the service's folder;
/// else the entry's. Never a default folder.
pub fn choose_uninstall_root(
    dir_arg: Option<&Path>,
    running_dir: Option<&Path>,
    service_dir: Option<&Path>,
    key_location: Option<&Path>,
    same: &dyn Fn(&Path, &Path) -> bool,
) -> Option<PathBuf> {
    if let Some(d) = dir_arg {
        return Some(d.to_path_buf());
    }
    if let Some(r) = running_dir {
        if service_dir.is_some_and(|s| same(r, s)) || key_location.is_some_and(|k| same(r, k)) {
            return Some(r.to_path_buf());
        }
    }
    service_dir.or(key_location).map(Path::to_path_buf)
}

const NO_INSTALLATION: &str =
    "No OmniIngest installation was found on this computer. To remove a copy in a folder, name it with /dir.";

/// Which machine-wide pieces belong to the installation in `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ownership {
    /// The registered service runs the executable in `root`.
    pub service: bool,
    /// The firewall rule: with our service, or when no service is left at all.
    pub firewall: bool,
    /// The Apps & Features entry names `root`; with no `InstallLocation`, only
    /// when the service is ours or there is none.
    pub key: bool,
    /// The folder of the installation the service belongs to instead.
    pub other: Option<PathBuf>,
}

/// What `/uninstall` of `root` may remove besides files. `same` compares folders.
pub fn ownership(
    root: &Path,
    service_exe: Option<&Path>,
    key_location: Option<&Path>,
    same: &dyn Fn(&Path, &Path) -> bool,
) -> Ownership {
    let service_dir = service_exe.and_then(Path::parent);
    let service = service_dir.is_some_and(|d| same(d, root));
    let none = service_exe.is_none();
    let key = match key_location {
        Some(k) => same(k, root),
        None => service || none,
    };
    Ownership { service, firewall: service || none, key, other: service_dir.filter(|_| !service).map(Path::to_path_buf) }
}

/// The closing line of an uninstall that went through: what was really removed.
pub fn uninstall_outcome(root: &Path, owned: &Ownership, files_removed: bool) -> String {
    match (owned.service, files_removed) {
        (true, true) => "✓ OmniIngest was removed.".into(),
        (true, false) => format!(
            "✓ The OmniIngest service was removed. Its folder {} held no program files, so no file was removed.",
            root.display()
        ),
        (false, true) if owned.other.is_some() => format!(
            "✓ The program files of the copy in {} were removed. The OmniIngest service belongs to another folder and was kept.",
            root.display()
        ),
        (false, true) => format!("✓ The program files in {} were removed; no OmniIngest service was registered.", root.display()),
        (false, false) => format!("  Nothing was installed in {}.", root.display()),
    }
}

/// What the interactive confirmation lists.
pub fn uninstall_summary(root: &Path, plan: &[Removal], remove_data: bool, owned: &Ownership) -> Vec<String> {
    let mut removes: Vec<&str> = Vec::new();
    if owned.service {
        removes.push("the Windows service");
    }
    if owned.firewall {
        removes.push("its firewall rule");
    }
    if owned.key {
        removes.push("the Apps & Features entry");
    }
    removes.push("the program files");
    let mut lines = vec![
        String::new(),
        format!("OmniIngest Setup {VERSION}: uninstall from {}", root.display()),
        format!("  Removes: {}.", removes.join(", ")),
    ];
    if let Some(other) = &owned.other {
        lines.push(format!("  Keeps the Windows service: it belongs to the installation in {}.", other.display()));
    }
    if remove_data {
        lines.push("  Also removes: config.json and the data, logs, temp and archive folders inside the install folder.".into());
    } else {
        lines.push("  Keeps: config.json, the database, the secrets, logs, temp and archive (add /remove-data to remove them).".into());
    }
    lines.push("  Never removes: the watchfolder or any video in it.".into());
    for r in plan {
        if let Removal::Keep { path, why } = r {
            lines.push(format!("  Keeps {}: {why}.", path.display()));
        }
    }
    lines
}

/// Everything `/uninstall` decides before it touches anything: the config
/// (with `/remove-data`, one that cannot be read refuses, since the watchfolder
/// would be unknown) and the removal plan with all of its refusals.
pub fn prepare_uninstall(root: &Path, remove_data: bool) -> Result<(AppPaths, Vec<Removal>)> {
    let paths = AppPaths::with_root(root, "config.json");
    let config = if paths.config.is_file() {
        match AppConfig::load_from_file(&paths.config) {
            Ok(c) => Some(c),
            Err(e) if remove_data => {
                return Err(fail(
                    ExitCode::Refused,
                    format!("{} cannot be read ({e:#}), so Setup cannot tell where the watchfolder is; nothing was removed.", paths.config.display()),
                ));
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let plan = removal_plan(root, &paths, config.as_ref(), remove_data)?;
    Ok((paths, plan))
}

async fn uninstall(
    dir: Option<&Path>,
    remove_data: bool,
    quiet: bool,
    running: &Path,
    console: &mut dyn Console,
    log: &mut SetupLog,
) -> Result<(Option<PathBuf>, Done)> {
    let service_exe = omni_service::installed_service_exe();
    let key_location = registry::read_install_location();
    let service_dir = service_exe.as_deref().and_then(Path::parent);
    let Some(root) = choose_uninstall_root(dir, running.parent(), service_dir, key_location.as_deref(), &same_folder) else {
        return Err(fail(ExitCode::Refused, NO_INSTALLATION));
    };
    let owned = ownership(&root, service_exe.as_deref(), key_location.as_deref(), &same_folder);
    // A registration whose folder is gone (or holds no program) is still removed;
    // only the files are skipped. A folder named with /dir must be an installation.
    let files_present = root.join("omni-ingest.exe").is_file();
    let with_files = files_present || dir.is_some();
    // Every refusal before the service is touched.
    let shown = if with_files { prepare_uninstall(&root, remove_data)?.1 } else { Vec::new() };
    if !with_files {
        log.say(&format!("  {} holds no OmniIngest program files; only its registration is removed.", root.display()));
    }
    if !quiet {
        for l in uninstall_summary(&root, &shown, remove_data, &owned) {
            log.say(&l);
        }
        if !console.confirm("Remove OmniIngest from this computer? [y/N] ") {
            return Err(fail(ExitCode::Cancelled, "Nothing was removed."));
        }
    }
    if owned.service {
        remove_service_registration(log)?;
    } else if let Some(other) = &owned.other {
        log.say(&format!("  The Windows service belongs to the installation in {}; it is kept.", other.display()));
    }
    if owned.firewall {
        remove_firewall_rule().await;
    }
    // The plan that runs is made now, with the service stopped: what was shown
    // could have changed under a running service (R3).
    let failed = if with_files {
        let (_paths, plan) = prepare_uninstall(&root, remove_data)?;
        execute_removals(&plan, &root, running, &omni_core::fsutil::is_link, &mut elevate::delete_at_restart, log)
    } else {
        0
    };
    if failed > 0 {
        // The Apps & Features entry stays, so the removal can be run again from there.
        return Err(fail(ExitCode::Failed, format!("{failed} item(s) could not be removed; see the lines above.")));
    }
    if owned.key {
        if let Err(e) = registry::remove_entry() {
            log.say(&format!("! The Apps & Features entry was not removed ({e:#})."));
        }
    } else {
        log.say("  The Apps & Features entry belongs to another installation; it is kept.");
    }
    if !remove_data && with_files {
        log.say(&format!("  The configuration, database and logs stay in {}.", root.display()));
    }
    log.say(&uninstall_outcome(&root, &owned, with_files));
    // After /remove-data the folder is going; a new setup.log would keep it.
    Ok(((!remove_data && with_files).then_some(root), Done::Ok))
}

/// The questions Setup itself asks at the console (the installer's own go
/// through `inquire`).
pub trait Console {
    /// Yes or no; anything but yes is no.
    fn confirm(&mut self, prompt: &str) -> bool;
    /// One line of text; `None` when there is no answer (end of input).
    fn ask(&mut self, prompt: &str) -> Option<String>;
}

/// A whole elevated-or-not run of a mode. `args` are the original arguments
/// (for the relaunch).
pub async fn run(
    args: &[String],
    mode: Mode,
    quiet: bool,
    elevated_child: bool,
    console: &mut dyn Console,
    log: &mut SetupLog,
) -> Finished {
    let running = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return Finished { code: ExitCode::Failed.code(), error: Some(format!("Setup cannot find its own file: {e}")) },
    };
    if !omni_service::can_install_services() {
        if elevated_child || quiet {
            return Finished {
                code: ExitCode::NeedsElevation.code(),
                error: Some(
                    "Setup needs administrator rights: start it from an elevated prompt, or from a deployment tool running as administrator."
                        .into(),
                ),
            };
        }
        // The child starts in another folder (an elevated session may not even
        // have this one's mapped drive): relative paths are made absolute here (R10, S7).
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let child = relaunch_args(args, &cwd);
        return match elevate::relaunch_elevated(&running, &command_line(&child)) {
            Ok(Relaunch::Declined) => Finished {
                code: ExitCode::Cancelled.code(),
                error: Some("Setup needs administrator rights; the Windows prompt was declined. Nothing was changed.".into()),
            },
            Ok(Relaunch::Exited(code)) => match decode_child_exit(code) {
                ChildResult::Exit(c) => Finished { code: c.code(), error: None },
                ChildResult::OpenAdmin { tls, port } => {
                    let url = admin_url(tls, port);
                    if !elevate::open_url(&url) {
                        log.say(&format!("Open {url} in a browser on this computer."));
                    }
                    Finished { code: ExitCode::Ok.code(), error: None }
                }
            },
            Err(e) => Finished { code: ExitCode::Failed.code(), error: Some(format!("{e:#}")) },
        };
    }

    let (name, root, result) = match &mode {
        Mode::Install { dir, account, no_firewall, answer } => {
            let r = install(&running, dir, account.clone(), *no_firewall, answer.as_deref(), quiet, log).await;
            ("install", Some(dir.clone()), r)
        }
        Mode::ChangeAccount { account, no_firewall } => {
            match change_account(account.as_deref(), *no_firewall, quiet, &running, console, log).await {
                Ok((dir, done)) => ("change-account", Some(dir), Ok(done)),
                Err(e) => ("change-account", None, Err(e)),
            }
        }
        Mode::Uninstall { dir, remove_data } => {
            match uninstall(dir.as_deref(), *remove_data, quiet, &running, console, log).await {
                Ok((root, done)) => ("uninstall", root, Ok(done)),
                Err(e) => ("uninstall", None, Err(e)),
            }
        }
    };
    let finished = match result {
        Ok(Done::Ok) => Finished { code: ExitCode::Ok.code(), error: None },
        Ok(Done::OpenAdmin { tls, port }) if elevated_child => Finished { code: encode_open(tls, port), error: None },
        Ok(Done::OpenAdmin { tls, port }) => {
            log.say(&format!("Open {} in a browser on this computer to finish the setup.", admin_url(tls, port)));
            Finished { code: ExitCode::Ok.code(), error: None }
        }
        Err(e) => Finished { code: exit_code_for(&e).code(), error: Some(format!("{e:#}")) },
    };
    if let Some(err) = &finished.error {
        log.lines.push(format!("✗ {err}"));
    }
    if let Some(root) = root.filter(|_| writes_setup_log(finished.code)) {
        let _ = append_setup_log(&root, &format!("OmniIngest Setup {VERSION} {name}"), &log.lines, now_unix());
    }
    finished
}

/// Whether a run ending with `code` is written to setup.log: only one that
/// changed the machine (done, failed part-way, rolled back). A refused folder
/// may belong to someone else, and a usage error or a cancel changed nothing (S8).
pub fn writes_setup_log(code: u32) -> bool {
    match decode_child_exit(code) {
        ChildResult::OpenAdmin { .. } => true,
        ChildResult::Exit(c) => matches!(c, ExitCode::Ok | ExitCode::Failed | ExitCode::RolledBack),
    }
}

/// `--pack`: this executable's stub and `folder` into `out`.
pub fn pack_self(folder: &Path, out: &Path) -> Result<payload::Packed> {
    let me = std::env::current_exe().context("Setup cannot find its own file")?;
    payload::pack(&me, folder, out)
}

/// `--verify`: the payload of `file` matches its checksum.
pub fn verify_file(file: &Path) -> Result<payload::PayloadSpan> {
    let v = open_setup(file)?.verify()?;
    Ok(v.span())
}

/// Flush stdout before a prompt (for main's console questions).
pub fn flush() {
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_cli::install::{RegistrationRestore, Restore, WayBack};

    /// A console with scripted answers that records what was asked.
    struct Scripted {
        answers: Vec<Option<String>>,
        asked: Vec<String>,
    }

    impl Console for Scripted {
        fn confirm(&mut self, prompt: &str) -> bool {
            self.asked.push(prompt.to_string());
            false
        }
        fn ask(&mut self, prompt: &str) -> Option<String> {
            self.asked.push(prompt.to_string());
            self.answers.remove(0)
        }
    }

    #[test]
    fn change_account_without_an_account_asks_and_empty_means_the_virtual_account() {
        let mut c = Scripted { answers: vec![Some("  \n".into()), Some(r"DOMAIN\svc_test".into()), None], asked: vec![] };
        assert_eq!(account_to_use(None, &mut c).unwrap(), omni_service::DEFAULT_SERVICE_ACCOUNT);
        assert_eq!(account_to_use(None, &mut c).unwrap(), r"DOMAIN\svc_test");
        // No answer (end of input) cancels: exit 6.
        let e = account_to_use(None, &mut c).unwrap_err();
        assert_eq!(exit_code_for(&e), ExitCode::Cancelled);
        assert_eq!(c.asked.len(), 3);
        assert!(c.asked[0].starts_with("Account the service should run as (empty = the built-in service account):"));
        // Given on the command line: nothing is asked.
        assert_eq!(account_to_use(Some("LocalSystem"), &mut c).unwrap(), "LocalSystem");
        assert_eq!(c.asked.len(), 3);
    }

    #[test]
    fn uninstall_refusals_come_from_the_preparation_before_anything_is_touched() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("OmniIngest");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("omni-ingest.exe"), b"x").unwrap();
        std::fs::write(root.join("config.json"), b"{ not json").unwrap();
        // With /remove-data an unreadable config.json refuses (exit 5); without, the default watchfolder is protected.
        let e = prepare_uninstall(&root, true).unwrap_err();
        assert_eq!(exit_code_for(&e), ExitCode::Refused);
        assert!(e.to_string().contains("cannot tell where the watchfolder is"), "{e}");
        assert!(prepare_uninstall(&root, false).is_ok());
        // A refusal of the plan is exit 5 too.
        let e = prepare_uninstall(&d.path().join("Photos"), false).unwrap_err();
        assert_eq!(exit_code_for(&e), ExitCode::Refused);
        assert!(root.join("config.json").exists() && root.join("omni-ingest.exe").exists());
    }

    #[test]
    fn the_admin_address_is_read_back_from_the_report() {
        assert_eq!(parse_admin_url("http://127.0.0.1:8080/admin"), Some((false, 8080)));
        assert_eq!(parse_admin_url("https://127.0.0.1:8443/admin"), Some((true, 8443)));
        assert_eq!(parse_admin_url("ftp://x:1/"), None);
    }

    #[test]
    fn quiet_refuses_a_plan_that_still_has_a_question() {
        let why = unanswerable(&[Question::NeedPassword { account: r"DOMAIN\svc_test".into() }]).unwrap();
        assert!(why.contains("without /quiet") && why.contains("/account default"), "{why}");
        assert_eq!(unanswerable(&[]), None);
    }

    #[test]
    fn the_uninstall_summary_names_what_goes_and_what_stays() {
        let root = Path::new(r"C:\OmniIngest");
        let keep = [Removal::Keep { path: root.to_path_buf(), why: "the watchfolder is inside it".into() }];
        let all = Ownership { service: true, firewall: true, key: true, other: None };
        let text = uninstall_summary(root, &keep, false, &all).join("\n");
        assert!(text.contains("Removes: the Windows service, its firewall rule, the Apps & Features entry, the program files."), "{text}");
        assert!(text.contains("Keeps: config.json") && text.contains("Never removes: the watchfolder"), "{text}");
        assert!(text.contains(r"Keeps C:\OmniIngest: the watchfolder is inside it."), "{text}");
        assert!(uninstall_summary(root, &[], true, &all).join("\n").contains("Also removes: config.json"));
        let other = Ownership { service: false, firewall: false, key: false, other: Some(PathBuf::from(r"D:\OtherOmni")) };
        let text = uninstall_summary(root, &[], false, &other).join("\n");
        assert!(text.contains("Removes: the program files.") && text.contains(r"installation in D:\OtherOmni"), "{text}");
    }

    /// S3: a Setup lying beside an unpacked release (Downloads, a deployment
    /// share) is not an installation; no default folder is ever guessed.
    #[test]
    fn uninstall_works_on_the_installation_and_never_on_the_folder_setup_sits_in() {
        let same = |a: &Path, b: &Path| a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy());
        let p = |s: &str| PathBuf::from(s);
        let downloads = p(r"C:\Users\op\Downloads\OmniIngest-1.0");
        let installed = p(r"C:\OmniIngest");
        let elsewhere = p(r"E:\Omni");
        let choose = |dir: Option<&PathBuf>, run: &PathBuf, svc: Option<&PathBuf>, key: Option<&PathBuf>| {
            choose_uninstall_root(dir.map(|d| d.as_path()), Some(run), svc.map(|d| d.as_path()), key.map(|d| d.as_path()), &same)
        };
        // The reviewer's case: Setup in Downloads next to a release, the service in C:\OmniIngest.
        assert_eq!(choose(None, &downloads, Some(&installed), Some(&installed)), Some(installed.clone()));
        // The installed stub itself, by the service or by the entry.
        assert_eq!(choose(None, &installed, Some(&installed), None), Some(installed.clone()));
        assert_eq!(choose(None, &installed, None, Some(&installed)), Some(installed.clone()));
        // No service: the entry's folder.
        assert_eq!(choose(None, &downloads, None, Some(&installed)), Some(installed.clone()));
        // Nothing registered anywhere: refused, never Downloads, never C:\OmniIngest by default.
        assert_eq!(choose(None, &downloads, None, None), None);
        // /dir is explicit.
        assert_eq!(choose(Some(&elsewhere), &downloads, Some(&installed), None), Some(elsewhere.clone()));
        // A gone folder is the same folder by name.
        assert!(same_folder(Path::new(r"C:\Gone\Omni\"), Path::new(r"c:\gone\omni")));
    }

    #[test]
    fn the_closing_line_says_what_was_really_removed() {
        let root = Path::new(r"C:\OmniIngest");
        let ours = Ownership { service: true, firewall: true, key: true, other: None };
        let copy = Ownership { service: false, firewall: false, key: false, other: Some(PathBuf::from(r"D:\Omni")) };
        let none = Ownership { service: false, firewall: true, key: true, other: None };
        assert_eq!(uninstall_outcome(root, &ours, true), "✓ OmniIngest was removed.");
        assert!(uninstall_outcome(root, &ours, false).starts_with("✓ The OmniIngest service was removed."));
        assert!(uninstall_outcome(root, &copy, true).contains("copy in C:\\OmniIngest"));
        assert!(uninstall_outcome(root, &none, true).contains("no OmniIngest service was registered"));
        for (o, f) in [(&copy, true), (&none, true), (&ours, false)] {
            assert_ne!(uninstall_outcome(root, o, f), "✓ OmniIngest was removed.");
        }
    }

    /// S8: a run that changed nothing leaves no setup.log behind.
    #[test]
    fn only_a_run_that_changed_the_machine_is_logged() {
        for c in [ExitCode::Ok, ExitCode::Failed, ExitCode::RolledBack] {
            assert!(writes_setup_log(c.code()), "{c:?}");
        }
        for c in [ExitCode::Usage, ExitCode::NeedsElevation, ExitCode::BadPayload, ExitCode::Refused, ExitCode::Cancelled] {
            assert!(!writes_setup_log(c.code()), "{c:?}");
        }
        assert!(writes_setup_log(encode_open(false, 8080)), "a new installation that asks for the browser");
    }

    /// R6: an uninstall from one folder never removes another installation's
    /// service, firewall rule or Apps & Features entry.
    #[test]
    fn uninstall_removes_only_the_service_and_entry_of_its_own_folder() {
        let same = |a: &Path, b: &Path| a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy());
        let root = Path::new(r"C:\OmniIngest");
        let here = Path::new(r"C:\OmniIngest\omni-ingest.exe");
        let there = Path::new(r"D:\OtherOmni\omni-ingest.exe");
        let o = |s: Option<&Path>, k: Option<&Path>| ownership(root, s, k, &same);
        // Ours, the entry ours or without a location.
        assert_eq!(o(Some(here), Some(root)), Ownership { service: true, firewall: true, key: true, other: None });
        assert_eq!(o(Some(here), None), Ownership { service: true, firewall: true, key: true, other: None });
        // The service is another folder's: kept, and so is an entry without a location.
        assert_eq!(
            o(Some(there), None),
            Ownership { service: false, firewall: false, key: false, other: Some(PathBuf::from(r"D:\OtherOmni")) }
        );
        // ... but an entry that names this folder is ours.
        assert!(o(Some(there), Some(root)).key);
        // No service at all: the leftover rule and entry go.
        assert_eq!(o(None, None), Ownership { service: false, firewall: true, key: true, other: None });
        // The entry points elsewhere: kept.
        assert!(!o(None, Some(Path::new(r"D:\OtherOmni"))).key);
        assert!(!o(Some(here), Some(Path::new(r"D:\OtherOmni"))).key);
    }

    /// P13.1b2 C: a way back that got the previous state running is exit 7;
    /// one that did not is exit 1, and the message says why the run failed.
    #[test]
    fn a_rolled_back_run_says_what_failed_and_exits_by_whether_the_previous_state_runs() {
        let report = InstallReport {
            kind: InstallKind::Upgrade { had_previous_exe: true },
            rolled_back: true,
            failure: Some("Cannot write config.json: access denied".into()),
            restored_running: true,
            way_back: None,
            admin_url: "http://127.0.0.1:8080/admin".into(),
            health: Vec::new(),
            logs: PathBuf::from(r"C:\OmniTest\logs"),
        };
        let e = rolled_back(&report, false);
        assert_eq!(exit_code_for(&e), ExitCode::RolledBack);
        assert_eq!(
            e.to_string(),
            "The upgrade failed (Cannot write config.json: access denied) and the previous version was put back; it is running."
        );
        let e = rolled_back(&report, true);
        assert_eq!(e.to_string(), "The account change failed (Cannot write config.json: access denied) and was undone; it is running.");
        let down = InstallReport { restored_running: false, ..report.clone() };
        let e = rolled_back(&down, true);
        assert_eq!(exit_code_for(&e), ExitCode::Failed);
        assert_eq!(
            e.to_string(),
            r"The account change failed (Cannot write config.json: access denied) and was undone; it did NOT start — see C:\OmniTest\logs."
        );
        // V1: the registration could not be put back; the service runs, but not
        // in its previous state: exit 1, and the message says what is there now.
        let back = WayBack {
            exe: Restore::Restored,
            config: Restore::Restored,
            registration: RegistrationRestore::NotRestored {
                exe: PathBuf::from(r"C:\OmniTest\omni-ingest.exe"),
                account: r"NT SERVICE\OmniIngestService".into(),
                why: "the previous account's password is not known to Setup".into(),
            },
            firewall: Restore::NotChanged,
            running: true,
        };
        let report = InstallReport { restored_running: back.restored_running(), way_back: Some(back), ..report };
        assert!(!report.restored_running);
        let e = rolled_back(&report, true);
        assert_eq!(exit_code_for(&e), ExitCode::Failed);
        assert_eq!(
            e.to_string(),
            r"The account change failed (Cannot write config.json: access denied). Put back: the previous omni-ingest.exe, config.json. The service is registered for C:\OmniTest\omni-ingest.exe as NT SERVICE\OmniIngestService: the previous account's password is not known to Setup. The service is running."
        );
        let e = rolled_back(&report, false);
        assert_eq!(exit_code_for(&e), ExitCode::Failed);
        assert!(e.to_string().starts_with("The upgrade failed (Cannot write config.json: access denied). Put back:"), "{e}");
        assert!(!e.to_string().contains("undone") && !e.to_string().contains("previous version was put back"), "{e}");
    }

    #[test]
    fn the_crate_version_is_the_release_version() {
        let root = include_str!("../../../Cargo.toml");
        let package = root.split("[package]").nth(1).expect("[package]");
        let line = package.lines().find(|l| l.trim_start().starts_with("version")).expect("version");
        let version = line.split('"').nth(1).expect("quoted version");
        assert_eq!(version, VERSION);
    }
}
