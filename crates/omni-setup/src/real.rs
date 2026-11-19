//! The pages' [`Backend`] over the real machine: what `/quiet` does
//! (`run.rs`), split into a read-only plan and a run (P13.1c). Only its
//! cleanup is exercised by tests: the rest registers services and changes
//! permissions.

use std::path::{Path, PathBuf};

use anyhow::Result;
use omni_cli::install::{
    execute, plan_answered, rolled_back_message, AccountDecision, Answers, InstallOptions, InstallPlan, Progress, Question,
    Refusal, Step,
};

use crate::backend::{Backend, Detected, Installed, Op, OpKind, Outcome, PlanSummary};
use crate::exit::{exit_code_for, ExitCode};
use crate::log::{append_setup_log, now_unix, SetupLog, SETUP_LOG};
use crate::payload::{open_setup, VerifiedPayload};
use crate::registry;
use crate::run::{
    after_change_account, after_install, installed_target, read_answer, uninstall_execute, uninstall_scope, uninstall_summary,
    unpack, with_answer, writes_setup_log, ExeProbe, VERSION,
};
use crate::ui::{zero_string, Feed, SummaryKind};
use crate::workdir::WorkDir;

/// The payload, unpacked once for every plan and run of this Setup.
struct Unpacked {
    verified: VerifiedPayload<std::fs::File>,
    exe: PathBuf,
}

/// How the work folder is removed (tests inject one that fails).
type Remover = fn(&Path) -> std::io::Result<()>;

pub struct RealBackend {
    /// This Setup's own file.
    running: PathBuf,
    no_firewall: bool,
    answer: Option<PathBuf>,
    rt: tokio::runtime::Runtime,
    unpacked: Option<Unpacked>,
    /// Where the payload was unpacked: removed by [`Backend::cleanup`] once
    /// the pages end; dropping it is only the safety net (Y2).
    work: Option<WorkDir>,
    remover: Remover,
    /// The setup.log this backend wrote last, and the run's name in it.
    last_log: Option<(PathBuf, &'static str)>,
}

/// Every installer line goes to setup.log and to the progress page.
struct Tee<'a> {
    log: &'a mut SetupLog,
    feed: &'a Feed,
}

impl Progress for Tee<'_> {
    fn line(&mut self, step: Step, text: &str) {
        self.log.lines.push(text.to_string());
        self.feed.line(step, text);
    }
}

/// An error of the installer as the pages' outcome.
fn stop(e: anyhow::Error) -> Outcome {
    if let Some(r) = e.chain().find_map(|c| c.downcast_ref::<Refusal>()) {
        return Outcome::Refused { notice: r.notice.clone(), message: format!("{e:#}"), account: r.account };
    }
    match exit_code_for(&e) {
        ExitCode::Refused => Outcome::Refused { notice: Vec::new(), message: format!("{e:#}"), account: false },
        code => Outcome::Failed { code, message: format!("{e:#}"), log: None },
    }
}

fn account_label(plan: &InstallPlan) -> String {
    match &plan.account {
        AccountDecision::Keep { current } => current.clone().unwrap_or_default(),
        AccountDecision::LocalSystem => "LocalSystem".into(),
        AccountDecision::Passwordless(n) | AccountDecision::NeedsPassword(n) => n.clone(),
    }
}

/// The summary page's facts of a plan.
fn summary(plan: &InstallPlan, kind: OpKind) -> PlanSummary {
    let kind = match kind {
        OpKind::ChangeAccount => SummaryKind::ChangeAccount,
        OpKind::Install if plan.target.is_upgrade() => SummaryKind::Upgrade,
        OpKind::Install => SummaryKind::Install,
    };
    // The console asks this one with No as the default; the Summary asks it
    // with Back as the default.
    let share = plan
        .questions
        .iter()
        .find_map(|q| match q {
            Question::ConfirmUncWithoutAccount { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    PlanSummary {
        kind,
        dir: plan.target.paths.root.clone(),
        account: account_label(plan),
        account_kept: matches!(plan.account, AccountDecision::Keep { .. }),
        firewall: plan.firewall,
        notes: plan.target.warnings.clone(),
        share,
        existing_database: plan.existing_database,
    }
}

/// The installer's options for `op` on the pages. Every account the pages
/// pass comes from the Account page: the built-in radio, the "as given on the
/// command line" radio, or a Windows account (which needs a password and is
/// never asked about). Picking "the built-in account (recommended)" is not
/// naming an account knowingly, so a share watchfolder still asks the share
/// question (P13.1c round 4). `/quiet` builds its own options without it.
pub(crate) fn options_for(op: &Op<'_>, dir: &Path, no_firewall: bool) -> InstallOptions {
    InstallOptions {
        dir: Some(dir.to_path_buf()),
        account: op.account.clone(),
        no_firewall,
        confirm_share_for_passwordless: true,
        ..Default::default()
    }
}

impl RealBackend {
    pub fn new(running: PathBuf, no_firewall: bool, answer: Option<PathBuf>) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        Ok(RealBackend {
            running,
            no_firewall,
            answer,
            rt,
            unpacked: None,
            work: None,
            remover: |p| std::fs::remove_dir_all(p),
            last_log: None,
        })
    }

    /// The release's executable, unpacked on first use.
    fn release_exe(&mut self) -> Result<PathBuf, Outcome> {
        if self.unpacked.is_none() {
            let (verified, work, exe) = unpack(&self.running).map_err(stop)?;
            self.unpacked = Some(Unpacked { verified, exe });
            self.work = Some(work);
        }
        Ok(self.unpacked.as_ref().map(|u| u.exe.clone()).unwrap_or_default())
    }

    /// The plan of `op` with the default watchfolder accepted (the web
    /// wizard asks for the real one), and the folder it works on.
    fn plan_op(&mut self, op: &Op<'_>, log: &mut SetupLog) -> Result<(InstallPlan, Answers, PathBuf), Outcome> {
        let (probe, dir) = match op.kind {
            OpKind::Install => (ExeProbe { exe: self.release_exe()? }, op.dir.clone()),
            OpKind::ChangeAccount => {
                let (exe, dir) = installed_target().map_err(stop)?;
                (ExeProbe { exe }, dir)
            }
        };
        let o = options_for(op, &dir, self.no_firewall);
        let o = match op.kind {
            OpKind::Install => {
                let applied = read_answer(self.answer.as_deref()).map_err(stop)?;
                with_answer(o, applied.as_ref(), &dir, &probe, log)
            }
            OpKind::ChangeAccount => o,
        };
        let mut answers = Answers::default();
        let mut plan = plan_answered(&o, &answers, &probe).map_err(stop)?;
        if let Some(default) = plan.needs_watchfolder() {
            answers.watchfolder = Some(default.to_string());
            plan = plan_answered(&o, &answers, &probe).map_err(stop)?;
        }
        Ok((plan, answers, dir))
    }

    /// Write setup.log for a run that changed the machine; the outcome then names it.
    fn write_log(&mut self, root: Option<&Path>, name: &'static str, log: &mut SetupLog, outcome: Outcome) -> Outcome {
        if let Some(err) = outcome.error() {
            log.lines.push(format!("✗ {err}"));
        }
        let Some(root) = root.filter(|_| writes_setup_log(outcome.code())) else { return outcome };
        let written = append_setup_log(root, &format!("OmniIngest Setup {VERSION} {name}"), &log.lines, now_unix());
        if matches!(written, Ok(true)) {
            self.last_log = Some((root.to_path_buf(), name));
        }
        match (outcome, written) {
            (Outcome::Failed { code, message, .. }, Ok(true)) => Outcome::Failed { code, message, log: Some(root.join(SETUP_LOG)) },
            (outcome, _) => outcome,
        }
    }
}

impl Backend for RealBackend {
    fn detect(&mut self) -> Detected {
        let service_exe = omni_service::installed_service_exe();
        let key = registry::read_install_location();
        let dir = service_exe.as_deref().and_then(Path::parent).map(Path::to_path_buf).or(key.clone());
        let installed = dir.map(|dir| Installed {
            dir,
            version: key.as_ref().and_then(|_| registry::read_display_version()),
            account: service_exe.as_ref().and_then(|_| omni_service::installed_service_account()),
            service: service_exe.is_some(),
        });
        let has_payload = open_setup(&self.running).map(|s| s.span().is_some()).unwrap_or(false);
        Detected { installed, has_payload }
    }

    fn plan(&mut self, op: &Op<'_>) -> Result<PlanSummary, Outcome> {
        // The answer file's notes are said when the plan runs, not on every visit.
        let mut quiet = SetupLog::new(false);
        let (plan, _, _) = self.plan_op(op, &mut quiet)?;
        Ok(summary(&plan, op.kind))
    }

    fn run(&mut self, op: &Op<'_>, feed: &Feed) -> Outcome {
        let mut log = SetupLog::new(false);
        let (plan, mut answers, dir) = match self.plan_op(op, &mut log) {
            Ok(p) => p,
            Err(outcome) => return outcome,
        };
        // Yes only from the affirmative button of a Summary that asked; a plan
        // that asks now but did not then is cancelled by `execute`, nothing changed.
        answers.confirm_unc = op.confirm_unc;
        answers.password = op.password.map(str::to_string);
        for l in plan.target.header() {
            log.say(&l);
        }
        let result = {
            let mut tee = Tee { log: &mut log, feed };
            self.rt.block_on(execute(&plan, &answers, &mut tee))
        };
        if let Some(p) = answers.password.take() {
            zero_string(p);
        }
        let change = op.kind == OpKind::ChangeAccount;
        let outcome = match result {
            Ok(report) if report.rolled_back => {
                Outcome::RolledBack { message: rolled_back_message(&report, change), running: report.restored_running }
            }
            Ok(report) => match (op.kind, self.unpacked.as_mut()) {
                (OpKind::Install, Some(u)) => Outcome::Done(after_install(&report, &dir, &mut u.verified, &mut log)),
                // `plan_op` unpacks before any install is planned.
                (OpKind::Install, None) => Outcome::Failed {
                    code: ExitCode::Failed,
                    message: "Setup lost its unpacked release before the install finished.".into(),
                    log: None,
                },
                (OpKind::ChangeAccount, _) => {
                    after_change_account(&dir, &self.running, &mut log);
                    Outcome::Done(crate::run::Done::Ok)
                }
            },
            Err(e) => stop(e),
        };
        self.write_log(Some(&dir), if change { "change-account" } else { "install" }, &mut log, outcome)
    }

    fn plan_removal(&mut self, dir: Option<&Path>, remove_data: bool) -> Result<Vec<String>, Outcome> {
        let scope = uninstall_scope(dir, remove_data, &self.running).map_err(stop)?;
        let mut lines = uninstall_summary(&scope.root, &scope.shown, remove_data, &scope.owned);
        if !scope.with_files {
            lines.push(format!("  {} holds no OmniIngest program files; only its registration is removed.", scope.root.display()));
        }
        Ok(lines)
    }

    fn run_removal(&mut self, dir: Option<&Path>, remove_data: bool, feed: &Feed) -> Outcome {
        // Decided again now: the page may have been open for a while.
        let scope = match uninstall_scope(dir, remove_data, &self.running) {
            Ok(s) => s,
            Err(e) => return stop(e),
        };
        let mut log = SetupLog::new(false);
        let result = {
            let mut tee = Tee { log: &mut log, feed };
            self.rt.block_on(uninstall_execute(&scope, remove_data, &self.running, &mut tee))
        };
        match result {
            Ok((root, sentence)) => self.write_log(root.as_deref(), "uninstall", &mut log, Outcome::Removed(sentence)),
            // As `/quiet`: a removal that failed part-way leaves no setup.log to keep the folder.
            Err(e) => stop(e),
        }
    }

    fn cleanup(&mut self) -> Option<String> {
        let line = self.work.as_mut()?.remove_with(&self.remover)?;
        // Into the setup.log of this Setup's last run too, when it wrote one.
        if let Some((root, name)) = &self.last_log {
            let header = format!("OmniIngest Setup {VERSION} {name} (cleanup)");
            let _ = append_setup_log(root, &header, std::slice::from_ref(&line), now_unix());
        }
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workdir::create_work_dir;
    use omni_cli::install::Probe;
    use omni_core::config::AppConfig;
    use omni_core::paths::{DriveKind, DriveLookup};

    const ROOT: &str = r"C:\OmniPagesTest";
    const SHARE: &str = r"\\192.168.4.50\ingest";

    struct NoDrives;

    impl DriveLookup for NoDrives {
        fn drive_kind(&self, _: char) -> DriveKind {
            DriveKind::Other
        }
        fn connection(&self, _: char) -> Option<String> {
            None
        }
        fn persistent_mapping(&self, _: char) -> Option<String> {
            None
        }
    }

    /// An installation in ROOT whose watchfolder is a NAS share, its service
    /// registered as `account`. Read-only, like every probe.
    struct OnShare {
        account: String,
    }

    impl OnShare {
        fn exe() -> PathBuf {
            Path::new(ROOT).join("omni-ingest.exe")
        }
        fn known(p: &Path) -> bool {
            let s = p.to_string_lossy().to_ascii_lowercase();
            s == Self::exe().to_string_lossy().to_ascii_lowercase() || s == format!(r"{}\config.json", ROOT.to_ascii_lowercase())
        }
    }

    impl Probe for OnShare {
        fn can_install_services(&self) -> bool {
            true
        }
        fn current_exe(&self) -> std::io::Result<PathBuf> {
            Ok(Self::exe())
        }
        fn installed_exe(&self) -> Option<PathBuf> {
            Some(Self::exe())
        }
        fn installed_account(&self) -> Option<String> {
            Some(self.account.clone())
        }
        fn exists(&self, p: &Path) -> bool {
            Self::known(p)
        }
        fn is_file(&self, p: &Path) -> bool {
            Self::known(p)
        }
        fn file_len(&self, p: &Path) -> Option<u64> {
            Self::known(p).then_some(1)
        }
        fn load_config(&self, _: &Path) -> Result<AppConfig> {
            let mut c = AppConfig::default();
            c.watchfolder_path = SHARE.into();
            Ok(c)
        }
        fn same_dir(&self, a: &Path, b: &Path) -> bool {
            a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
        }
        fn drives(&self) -> &dyn DriveLookup {
            &NoDrives
        }
        fn foreign_owner(&self, _: &Path) -> Option<String> {
            None
        }
        fn account_exists(&self, _: &str) -> bool {
            true
        }
        fn is_link(&self, _: &Path) -> bool {
            false
        }
        fn swappable_ancestor(&self, _: &Path) -> Option<(PathBuf, String)> {
            None
        }
        fn first_link_under(&self, _: &Path) -> Option<PathBuf> {
            None
        }
        fn untrusted_owner(&self, _: &Path) -> Option<String> {
            None
        }
        fn writable_by(&self, _: &Path) -> Option<String> {
            None
        }
    }

    fn pages_plan(account: &str, password: Option<&str>, installed_as: &str) -> PlanSummary {
        let op = Op { kind: OpKind::ChangeAccount, dir: ROOT.into(), account: Some(account.into()), password, confirm_unc: false };
        let o = options_for(&op, Path::new(ROOT), false);
        let plan = plan_answered(&o, &Answers::default(), &OnShare { account: installed_as.into() }).unwrap();
        summary(&plan, op.kind)
    }

    /// Round 4: on a NAS watchfolder, the built-in account picked on the
    /// Account page still asks the share question (the Summary then defaults
    /// to Back); a Windows account gives the normal Summary.
    #[test]
    fn the_built_in_account_picked_on_the_pages_asks_the_share_question() {
        let built_in = pages_plan(omni_service::DEFAULT_SERVICE_ACCOUNT, None, "LocalSystem");
        assert_eq!(built_in.kind, SummaryKind::ChangeAccount);
        assert!(!built_in.share.is_empty(), "{built_in:?}");
        assert!(built_in.share[0].contains(SHARE), "{:?}", built_in.share);
        let page = crate::ui::SummaryPage {
            kind: built_in.kind,
            version: VERSION.into(),
            dir: built_in.dir.clone(),
            account: built_in.account.clone(),
            account_kept: built_in.account_kept,
            firewall: built_in.firewall,
            notes: built_in.notes.clone(),
            share: built_in.share.clone(),
        };
        assert_eq!(crate::ui::render_summary(&page, crate::texts::Lang::En).default_button, crate::ui::ID_BACK);
        // LocalSystem, as given on the command line, too.
        assert!(!pages_plan("LocalSystem", None, omni_service::DEFAULT_SERVICE_ACCOUNT).share.is_empty());
        // A Windows account: the normal Summary.
        assert!(pages_plan(r"NEWSROOM\svc_test", Some("x"), "LocalSystem").share.is_empty());
    }

    /// Y2: the work folder is removed by `cleanup`, and a folder that cannot
    /// be removed comes back as a line, also written into the last setup.log.
    #[test]
    fn a_work_folder_that_cannot_be_removed_comes_back_from_cleanup() {
        let d = tempfile::tempdir().unwrap();
        let mut b = RealBackend::new(d.path().join("OmniIngest-Setup.exe"), false, None).unwrap();
        assert_eq!(b.cleanup(), None, "nothing unpacked, nothing to say");
        let root = d.path().join("Omni");
        std::fs::create_dir_all(&root).unwrap();
        b.work = Some(create_work_dir(d.path(), &|p: &Path| std::fs::create_dir(p)).unwrap());
        b.last_log = Some((root.clone(), "install"));
        b.remover = |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let line = b.cleanup().expect("the line");
        assert!(line.contains("delete it by hand"), "{line}");
        let log = std::fs::read_to_string(root.join(SETUP_LOG)).unwrap();
        assert!(log.contains("install (cleanup)") && log.contains(&line), "{log}");
        // The real remover then takes it away.
        b.remover = |p| std::fs::remove_dir_all(p);
        let work = b.work.as_ref().unwrap().path().to_path_buf();
        assert_eq!(b.cleanup(), None);
        assert!(!work.exists());
    }
}
