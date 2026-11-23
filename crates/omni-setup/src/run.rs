//! What Setup does once its command line is parsed: elevate (or ask the
//! unelevated parent to), then install, change the account or uninstall by
//! driving `omni_cli::install`, and end with one exit code. `/quiet` runs
//! [`run_quiet`]; everything else is Setup's pages ([`run_pages`], P13.1c).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use omni_cli::install::{
    execute, plan_answered, remove_firewall_rule, remove_service_registration, rolled_back_message, same_dir, Answers,
    InstallKind, InstallOptions, InstallReport, Probe, Progress, Question, Refusal, Step, SystemProbe,
};
use omni_core::answer_file::Applied;
use omni_core::config::AppConfig;
use omni_core::paths::{AppPaths, DriveLookup};

use crate::args::{command_line, relaunch_args, Mode};
use crate::backend::Backend;
use crate::elevate::{self, Relaunch};
use crate::exit::{admin_url, coded, decode_child_exit, encode_open, exit_code_for, fail, ChildResult, ExitCode};
use crate::flow::{self, Start};
use crate::log::{append_setup_log, now_unix, yyyymmdd, SetupLog};
use crate::payload::{self, open_setup, PayloadError, VerifiedPayload};
use crate::registry::{self, SETUP_EXE};
use crate::removal::{execute_removals, removal_plan, Removal};
use crate::texts::{tr, Lang, T};
use crate::ui::Ui;
use crate::workdir::{create_secure_work_dir, WorkDir};

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
             to type it in the Windows credential prompt, or use the virtual account (/account default)."
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

/// A rolled-back run as an error: exit 7 when the previous state is back and
/// running, 1 when the way back itself did not get there (`account_change`:
/// `/change-account`'s wording).
pub fn rolled_back(report: &InstallReport, account_change: bool) -> anyhow::Error {
    let code = if report.restored_running { ExitCode::RolledBack } else { ExitCode::Failed };
    fail(code, rolled_back_message(report, account_change))
}

/// Replace `<dir>\OmniIngest-Setup.exe` with `write`'s bytes, through a temporary name.
pub fn replace_setup_exe(dir: &Path, write: &mut dyn FnMut(&mut std::fs::File) -> std::result::Result<u64, PayloadError>) -> Result<()> {
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

/// `/answer`'s file read and applied to the defaults; `None` without one.
pub fn read_answer(answer: Option<&Path>) -> Result<Option<Applied>> {
    let Some(path) = answer else { return Ok(None) };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Cannot read the answer file {}", path.display()))
        .map_err(|e| coded(ExitCode::Refused, e))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("The answer file {} is not JSON", path.display()))
        .map_err(|e| coded(ExitCode::Refused, e))?;
    Ok(Some(omni_core::answer_file::apply(&doc, AppConfig::default())?))
}

/// The answer file's configuration for a new installation in `dir`. The rule
/// is "no config.json yet", not "nothing installed": an uninstall that kept
/// the data leaves one behind (R7).
pub fn with_answer(o: InstallOptions, applied: Option<&Applied>, dir: &Path, probe: &dyn Probe, log: &mut SetupLog) -> InstallOptions {
    let Some(applied) = applied else { return o };
    if probe.exists(&AppPaths::with_root(dir, "config.json").config) {
        log.say("  The answer file is used only for a new installation (no config.json yet); the existing configuration is kept.");
        return o;
    }
    for note in &applied.notes {
        log.say(&format!("  {note}"));
    }
    o.with_base_config(applied.config.clone())
}

/// The payload checked and unpacked into a new work folder: the verified
/// payload (for the stub copy), the folder, and the release's executable.
pub fn unpack(exe: &Path) -> Result<(VerifiedPayload<std::fs::File>, WorkDir, PathBuf)> {
    let mut verified = open_setup(exe)?.verify()?;
    let work = create_secure_work_dir().context("Cannot create Setup's work folder").map_err(|e| coded(ExitCode::Failed, e))?;
    let extract = |dir: &Path| -> Result<PathBuf> {
        verified.extract(dir)?;
        let release_exe = dir.join("omni-ingest.exe");
        if !release_exe.is_file() {
            return Err(PayloadError::Archive("it holds no omni-ingest.exe".into()).into());
        }
        Ok(release_exe)
    };
    let (work, release_exe) = extract_in(work, extract, &|p| std::fs::remove_dir_all(p))?;
    Ok((verified, work, release_exe))
}

/// Run `extract` in `work`. When it fails, the folder is removed now, and a
/// line saying it could not be is carried in the error, which `/quiet`
/// prints and the pages show (P13.1c Y2); the exit code is the error's own.
pub fn extract_in(
    mut work: WorkDir,
    extract: impl FnOnce(&Path) -> Result<PathBuf>,
    remove: &dyn Fn(&Path) -> std::io::Result<()>,
) -> Result<(WorkDir, PathBuf)> {
    match extract(work.path()) {
        Ok(exe) => Ok((work, exe)),
        Err(e) => Err(match work.remove_with(remove) {
            None => e,
            Some(line) => fail(exit_code_for(&e), format!("{e:#}\n{line}")),
        }),
    }
}

/// After an install that did not roll back: this Setup's stub into the
/// folder, the Apps & Features entry, and whether to open the admin page.
pub fn after_install(report: &InstallReport, dir: &Path, verified: &mut VerifiedPayload<std::fs::File>, log: &mut SetupLog) -> Done {
    if let Err(e) = replace_setup_exe(dir, &mut |f| verified.write_stub(f)) {
        log.say(&format!("! {SETUP_EXE} was not copied to {} ({e:#}); keep this Setup to uninstall later.", dir.display()));
    }
    write_apps_entry(dir, log);
    // P13.9: the Start-menu entry for signing a site in, without a terminal.
    let line = crate::shortcut::install_line(&crate::shortcut::install(dir));
    if !line.is_empty() {
        log.say(&line);
    }
    match (report.kind, parse_admin_url(&report.admin_url)) {
        (InstallKind::Fresh, Some((tls, port))) => Done::OpenAdmin { tls, port },
        _ => Done::Ok,
    }
}

async fn install(exe: &Path, dir: &Path, account: Option<String>, no_firewall: bool, answer: Option<&Path>, log: &mut SetupLog) -> Result<Done> {
    let answered = read_answer(answer)?;
    let (mut verified, mut work, release_exe) = unpack(exe)?;
    let result = async {
        let probe = ExeProbe { exe: release_exe };
        let o = InstallOptions { dir: Some(dir.to_path_buf()), account, yes: true, no_firewall, ..Default::default() };
        let o = with_answer(o, answered.as_ref(), dir, &probe, log);
        let report = quiet_install(&o, &probe, log).await?;
        if report.rolled_back {
            return Err(rolled_back(&report, false));
        }
        Ok(after_install(&report, dir, &mut verified, log))
    }
    .await;
    if let Some(line) = work.remove() {
        log.say(&line);
    }
    result
}

/// The registered service's executable and folder, for `/change-account`.
pub fn installed_target() -> Result<(PathBuf, PathBuf)> {
    let Some(exe) = omni_service::installed_service_exe() else {
        return Err(fail(ExitCode::Refused, "The OmniIngest service is not installed on this computer; there is no account to change."));
    };
    let dir = exe.parent().context("The installed service has no folder").map_err(|e| coded(ExitCode::Refused, e))?.to_path_buf();
    if !exe.is_file() {
        return Err(fail(ExitCode::Refused, format!("The service points at {}, which does not exist.", exe.display())));
    }
    Ok((exe, dir))
}

async fn change_account(account: &str, no_firewall: bool, running: &Path, log: &mut SetupLog) -> Result<(PathBuf, Done)> {
    let (exe, dir) = installed_target()?;
    let probe = ExeProbe { exe };
    let o = InstallOptions { dir: Some(dir.clone()), account: Some(account.to_string()), yes: true, no_firewall, ..Default::default() };
    let report = quiet_install(&o, &probe, log).await?;
    if report.rolled_back {
        return Err(rolled_back(&report, true));
    }
    after_change_account(&dir, running, log);
    Ok((dir, Done::Ok))
}

/// After an account change that did not roll back: an installation made by
/// `omni-ingest install` has no Setup in it yet; the Apps & Features entry
/// would point at nothing (R11).
pub fn after_change_account(dir: &Path, running: &Path, log: &mut SetupLog) {
    let stub = dir.join(SETUP_EXE);
    if !same_dir(&stub, running) {
        let copied = open_setup(running)
            .map_err(anyhow::Error::from)
            .and_then(|mut me| replace_setup_exe(dir, &mut |f| me.write_stub(f)));
        if let Err(e) = copied {
            log.say(&format!("! {SETUP_EXE} was not copied to {} ({e:#}).", dir.display()));
        }
    }
    if stub.is_file() {
        write_apps_entry(dir, log);
    } else {
        log.say(&format!("  No {SETUP_EXE} in {}: the Apps & Features entry was not written.", dir.display()));
    }
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

/// What `/uninstall` works on, decided before anything is touched.
#[derive(Debug, Clone)]
pub struct UninstallScope {
    pub root: PathBuf,
    pub owned: Ownership,
    /// The program files are removed too: they are there, or `/dir` named the folder.
    pub with_files: bool,
    /// The removal plan as it stands now, for the confirmation.
    pub shown: Vec<Removal>,
}

/// The installation `/uninstall` removes and every refusal, before the
/// service is touched. Reads the machine; changes nothing.
pub fn uninstall_scope(dir: Option<&Path>, remove_data: bool, running: &Path) -> Result<UninstallScope> {
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
    let shown = if with_files { prepare_uninstall(&root, remove_data)?.1 } else { Vec::new() };
    Ok(UninstallScope { root, owned, with_files, shown })
}

/// Remove what `scope` names: the folder to write setup.log into (none after
/// `/remove-data`), and the closing sentence ([`uninstall_outcome`]).
pub async fn uninstall_execute(
    scope: &UninstallScope,
    remove_data: bool,
    running: &Path,
    log: &mut dyn Progress,
) -> Result<(Option<PathBuf>, String)> {
    fn say(p: &mut dyn Progress, text: &str) {
        p.line(Step::Plan, text);
    }
    let UninstallScope { root, owned, with_files, .. } = scope;
    let (root, with_files) = (root.as_path(), *with_files);
    if !with_files {
        say(log, &format!("  {} holds no OmniIngest program files; only its registration is removed.", root.display()));
    }
    if owned.service {
        remove_service_registration(log)?;
    } else if let Some(other) = &owned.other {
        say(log, &format!("  The Windows service belongs to the installation in {}; it is kept.", other.display()));
    }
    if owned.firewall {
        remove_firewall_rule().await;
    }
    // The plan that runs is made now, with the service stopped: what was shown
    // could have changed under a running service (R3).
    let failed = if with_files {
        let (_paths, plan) = prepare_uninstall(root, remove_data)?;
        execute_removals(&plan, root, running, &omni_core::fsutil::is_link, &mut elevate::delete_at_restart, log)
    } else {
        0
    };
    if failed > 0 {
        // The Apps & Features entry stays, so the removal can be run again from there.
        return Err(fail(ExitCode::Failed, format!("{failed} item(s) could not be removed; see the lines above.")));
    }
    if owned.key {
        if let Err(e) = registry::remove_entry() {
            say(log, &format!("! The Apps & Features entry was not removed ({e:#})."));
        }
    } else {
        say(log, "  The Apps & Features entry belongs to another installation; it is kept.");
    }
    // P13.9: the Start-menu sign-in entry goes with the installation it points at.
    if let Some(line) = crate::shortcut::remove_line(&crate::shortcut::remove(root)) {
        say(log, &line);
    }
    if !remove_data && with_files {
        say(log, &format!("  The configuration, database and logs stay in {}.", root.display()));
    }
    let outcome = uninstall_outcome(root, owned, with_files);
    say(log, &outcome);
    // After /remove-data the folder is going; a new setup.log would keep it.
    Ok(((!remove_data && with_files).then(|| root.to_path_buf()), outcome))
}

async fn uninstall(dir: Option<&Path>, remove_data: bool, running: &Path, log: &mut SetupLog) -> Result<(Option<PathBuf>, Done)> {
    let scope = uninstall_scope(dir, remove_data, running)?;
    let (root, _) = uninstall_execute(&scope, remove_data, running, log).await?;
    Ok((root, Done::Ok))
}

const NEEDS_ELEVATION: &str =
    "Setup needs administrator rights: start it from an elevated prompt, or from a deployment tool running as administrator.";

/// `/quiet`: a whole run of a mode with nothing asked and no page shown (it
/// has no [`Ui`] to show one with). Never relaunches itself: not elevated is exit 3.
pub async fn run_quiet(mode: Mode, elevated_child: bool, log: &mut SetupLog) -> Finished {
    let running = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return Finished { code: ExitCode::Failed.code(), error: Some(format!("Setup cannot find its own file: {e}")) },
    };
    if !omni_service::can_install_services() {
        return Finished { code: ExitCode::NeedsElevation.code(), error: Some(NEEDS_ELEVATION.into()) };
    }

    let (name, root, result) = match &mode {
        Mode::Install { dir, account, no_firewall, answer } => {
            let r = install(&running, dir, account.clone(), *no_firewall, answer.as_deref(), log).await;
            ("install", Some(dir.clone()), r)
        }
        Mode::ChangeAccount { account, no_firewall } => {
            // `parse_args` refuses `/change-account /quiet` without `/account`.
            let r = match account.as_deref() {
                Some(a) => change_account(a, *no_firewall, &running, log).await,
                None => Err(fail(ExitCode::Usage, "/change-account /quiet needs /account <name>.")),
            };
            match r {
                Ok((dir, done)) => ("change-account", Some(dir), Ok(done)),
                Err(e) => ("change-account", None, Err(e)),
            }
        }
        Mode::Uninstall { dir, remove_data } => match uninstall(dir.as_deref(), *remove_data, &running, log).await {
            Ok((root, done)) => ("uninstall", root, Ok(done)),
            Err(e) => ("uninstall", None, Err(e)),
        },
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

/// Setup's pages (P13.1c). Not elevated: relaunch elevated and wait (the
/// child shows the pages), then open the admin page for a new installation
/// as this unelevated user. Elevated: the pages over the real machine.
/// `new_ui` is called only when pages are shown; `tell` shows one message
/// without a console (a declined UAC prompt).
pub fn run_pages(
    args: &[String],
    mode: Mode,
    lang: Lang,
    elevated_child: bool,
    new_ui: &mut dyn FnMut() -> Box<dyn Ui>,
    tell: &mut dyn FnMut(&str),
) -> Finished {
    let running = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            let error = format!("Setup cannot find its own file: {e}");
            tell(&error);
            return Finished { code: ExitCode::Failed.code(), error: Some(error) };
        }
    };
    if !omni_service::can_install_services() {
        if elevated_child {
            tell(tr(lang, T::BoxNotElevated));
            return Finished { code: ExitCode::NeedsElevation.code(), error: Some(NEEDS_ELEVATION.into()) };
        }
        // The child starts in another folder (an elevated session may not even
        // have this one's mapped drive): relative paths are made absolute here (R10, S7).
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let child = relaunch_args(args, &cwd, lang);
        return match elevate::relaunch_elevated(&running, &command_line(&child)) {
            Ok(Relaunch::Declined) => {
                tell(tr(lang, T::BoxDeclined));
                Finished { code: ExitCode::Cancelled.code(), error: Some(tr(lang, T::BoxDeclined).into()) }
            }
            Ok(Relaunch::Exited(code)) => match decode_child_exit(code) {
                ChildResult::Exit(c) => Finished { code: c.code(), error: None },
                ChildResult::OpenAdmin { tls, port } => {
                    // The Finished page told the address too.
                    let _ = elevate::open_url(&admin_url(tls, port));
                    Finished { code: ExitCode::Ok.code(), error: None }
                }
            },
            Err(e) => {
                let error = format!("{} {e:#}", tr(lang, T::BoxNoStart));
                tell(&error);
                Finished { code: ExitCode::Failed.code(), error: Some(error) }
            }
        };
    }
    let (no_firewall, answer) = match &mode {
        Mode::Install { no_firewall, answer, .. } => (*no_firewall, answer.clone()),
        Mode::ChangeAccount { no_firewall, .. } => (*no_firewall, None),
        Mode::Uninstall { .. } => (false, None),
    };
    let mut backend = match crate::real::RealBackend::new(running, no_firewall, answer) {
        Ok(b) => b,
        Err(e) => {
            let error = format!("{e:#}");
            tell(&error);
            return Finished { code: ExitCode::Failed.code(), error: Some(error) };
        }
    };
    pages_with(&mut *new_ui(), &mut backend, Start::from_mode(&mode), elevated_child, tell)
}

/// The pages over `backend`. However they end (Cancel, refusal, failure,
/// success), the backend's work folder is removed then, and a line saying it
/// could not be is told (`tell`: a message box once the pages are closed; Y2).
/// An elevated run that is nobody's child has no unelevated parent to open
/// the admin page for it: the page told the address, and it exits 0 (an
/// elevated browser is not opened).
pub fn pages_with(
    ui: &mut dyn Ui,
    backend: &mut dyn Backend,
    start: Start,
    elevated_child: bool,
    tell: &mut dyn FnMut(&str),
) -> Finished {
    let finished = flow::interactive(ui, backend, start);
    if let Some(line) = backend.cleanup() {
        tell(&line);
    }
    match decode_child_exit(finished.code) {
        ChildResult::OpenAdmin { .. } if !elevated_child => Finished { code: ExitCode::Ok.code(), error: None },
        _ => finished,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use omni_cli::install::{RegistrationRestore, Restore, WayBack};

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

    /// Y2: a failed extraction removes the work folder now; a folder that
    /// cannot be removed is said in the error `/quiet` prints, whose exit code
    /// stays the extraction's.
    #[test]
    fn a_failed_extraction_says_when_its_work_folder_cannot_be_removed() {
        let d = tempfile::tempdir().unwrap();
        let new_work = || crate::workdir::create_work_dir(d.path(), &|p: &Path| std::fs::create_dir(p)).unwrap();
        let corrupt = |_: &Path| -> Result<PathBuf> { Err(PayloadError::Corrupt.into()) };
        let denied = |_: &Path| -> std::io::Result<()> { Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)) };
        let e = extract_in(new_work(), corrupt, &denied).map(|_| ()).unwrap_err();
        assert_eq!(exit_code_for(&e), ExitCode::BadPayload);
        let text = format!("{e:#}");
        assert!(text.contains("delete it by hand"), "{text}");
        assert!(text.starts_with(&PayloadError::Corrupt.to_string()), "{text}");
        // Removed: the error is the extraction's own, and the folder is gone.
        let w = new_work();
        let path = w.path().to_path_buf();
        let e = extract_in(w, corrupt, &|p| std::fs::remove_dir_all(p)).map(|_| ()).unwrap_err();
        assert_eq!(format!("{e:#}"), PayloadError::Corrupt.to_string());
        assert!(!path.exists());
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
            existing_database: true,
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
