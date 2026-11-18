//! The pages' flow over a scripted [`Ui`] and a recording [`Backend`]: no
//! window, no service, nothing on the machine (P13.1c).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_cli::install::{rolled_back_message, InstallKind, InstallReport, Step};

use super::*;
use crate::backend::{DemoBackend, Detected, Scenario};
use crate::exit::encode_open;
use crate::texts::Lang;
use crate::ui::{
    account_answer, nav, remove_answer, render_account, render_remove, render_summary, render_welcome, welcome_choice, Credentials,
    Feed, Job,
};

const PASSWORD: &str = "Tr0ub4dor-test-only";
const DEFAULT: &str = omni_service::DEFAULT_SERVICE_ACCOUNT;

/// Answers each page from its queue and records what it was shown.
#[derive(Default)]
struct Scripted {
    welcome: VecDeque<WelcomeChoice>,
    folders: VecDeque<Option<PathBuf>>,
    account: VecDeque<AccountAnswer>,
    creds: VecDeque<Option<&'static str>>,
    summary: VecDeque<Nav>,
    remove: VecDeque<RemoveAnswer>,
    /// Page names, in order.
    shown: Vec<&'static str>,
    welcomes: Vec<WelcomePage>,
    accounts: Vec<AccountPage>,
    summaries: Vec<SummaryPage>,
    removes: Vec<RemovePage>,
    finished: Vec<FinishedPage>,
}

fn next<T>(q: &mut VecDeque<T>, page: &str) -> T {
    q.pop_front().unwrap_or_else(|| panic!("the {page} page was not expected here"))
}

impl Ui for Scripted {
    fn welcome(&mut self, page: &WelcomePage) -> WelcomeChoice {
        self.shown.push("welcome");
        self.welcomes.push(page.clone());
        next(&mut self.welcome, "welcome")
    }
    fn pick_folder(&mut self, _current: &Path) -> Option<PathBuf> {
        self.shown.push("folder");
        next(&mut self.folders, "folder picker")
    }
    fn account(&mut self, page: &AccountPage) -> AccountAnswer {
        self.shown.push("account");
        self.accounts.push(page.clone());
        next(&mut self.account, "account")
    }
    fn credentials(&mut self, _request: &CredRequest) -> Option<Credentials> {
        self.shown.push("credentials");
        next(&mut self.creds, "credential prompt")
            .map(|user| Credentials { user: user.into(), password: Secret::new(PASSWORD.into()) })
    }
    fn summary(&mut self, page: &SummaryPage) -> Nav {
        self.shown.push("summary");
        self.summaries.push(page.clone());
        next(&mut self.summary, "summary")
    }
    fn progress(&mut self, _page: &ProgressPage, job: Job<'_>) -> Outcome {
        self.shown.push("progress");
        job(&Feed::default())
    }
    fn remove(&mut self, page: &RemovePage) -> RemoveAnswer {
        self.shown.push("remove");
        self.removes.push(page.clone());
        next(&mut self.remove, "remove")
    }
    fn finished(&mut self, page: &FinishedPage) {
        self.shown.push("finished");
        self.finished.push(page.clone());
    }
}

/// Records every call; `run*` are the only ones that change a machine.
struct Recorder {
    detected: Detected,
    plans: VecDeque<Result<PlanSummary, Outcome>>,
    runs: VecDeque<Outcome>,
    calls: Vec<String>,
    /// The share question every default plan with the built-in account asks.
    share: Vec<String>,
    /// Every default plan finds a database in the folder.
    existing_database: bool,
    /// What `cleanup` says (a work folder that could not be removed).
    cleanup: Option<String>,
}

impl Recorder {
    fn fresh() -> Self {
        Recorder {
            detected: Detected { installed: None, has_payload: true },
            plans: VecDeque::new(),
            runs: VecDeque::new(),
            calls: Vec::new(),
            share: Vec::new(),
            existing_database: false,
            cleanup: None,
        }
    }

    fn installed() -> Self {
        let installed = Installed {
            dir: PathBuf::from(r"E:\Omni"),
            version: Some("0.9.4".into()),
            account: Some(DEFAULT.into()),
            service: true,
        };
        Recorder { detected: Detected { installed: Some(installed), has_payload: true }, ..Recorder::fresh() }
    }

    fn machine_calls(&self) -> Vec<&String> {
        self.calls.iter().filter(|c| c.starts_with("run")).collect()
    }

    fn op(op: &Op<'_>) -> String {
        format!(
            "{:?} {} account={} pw={}{}",
            op.kind,
            op.dir.display(),
            op.account.as_deref().unwrap_or("-"),
            if op.password == Some(PASSWORD) { "given" } else if op.password.is_some() { "OTHER" } else { "-" },
            if op.confirm_unc { " confirm_unc" } else { "" }
        )
    }
}

impl Backend for Recorder {
    fn detect(&mut self) -> Detected {
        self.calls.push("detect".into());
        self.detected.clone()
    }
    fn plan(&mut self, op: &Op<'_>) -> Result<PlanSummary, Outcome> {
        self.calls.push(format!("plan {}", Self::op(op)));
        self.plans.pop_front().unwrap_or_else(|| {
            let kind = match (op.kind, self.detected.installed.is_some()) {
                (OpKind::ChangeAccount, _) => SummaryKind::ChangeAccount,
                (OpKind::Install, true) => SummaryKind::Upgrade,
                (OpKind::Install, false) => SummaryKind::Install,
            };
            let account = op.account.clone().unwrap_or_else(|| DEFAULT.into());
            let built_in = account == DEFAULT;
            Ok(PlanSummary {
                kind,
                dir: op.dir.clone(),
                account,
                account_kept: op.account.is_none() && kind == SummaryKind::Upgrade,
                firewall: Some(8080),
                notes: Vec::new(),
                share: if built_in { self.share.clone() } else { Vec::new() },
                existing_database: self.existing_database,
            })
        })
    }
    fn run(&mut self, op: &Op<'_>, feed: &Feed) -> Outcome {
        self.calls.push(format!("run {}", Self::op(op)));
        feed.line(Step::Done, "done");
        self.runs.pop_front().expect("a run was not expected")
    }
    fn plan_removal(&mut self, dir: Option<&Path>, remove_data: bool) -> Result<Vec<String>, Outcome> {
        self.calls.push(format!("plan_removal {dir:?} {remove_data}"));
        match self.plans.pop_front() {
            Some(Err(o)) => Err(o),
            _ => Ok(vec![format!("  plan for remove_data={remove_data}")]),
        }
    }
    fn run_removal(&mut self, dir: Option<&Path>, remove_data: bool, _feed: &Feed) -> Outcome {
        self.calls.push(format!("run_removal {dir:?} {remove_data}"));
        self.runs.pop_front().expect("a removal was not expected")
    }
    fn cleanup(&mut self) -> Option<String> {
        self.calls.push("cleanup".into());
        self.cleanup.clone()
    }
}

fn start() -> Start {
    Start::from_mode(&Mode::Install { dir: PathBuf::from(r"C:\OmniIngest"), account: None, no_firewall: false, answer: None })
}

fn open_admin() -> Outcome {
    Outcome::Done(Done::OpenAdmin { tls: false, port: 8080 })
}

#[test]
fn a_fresh_install_with_the_defaults_plans_and_runs_the_default_folder_and_the_built_in_account() {
    let mut ui = Scripted {
        welcome: [WelcomeChoice::Install].into(),
        account: [AccountAnswer::BuiltIn].into(),
        summary: [Nav::Confirm].into(),
        ..Default::default()
    };
    let mut b = Recorder::fresh();
    b.runs.push_back(open_admin());
    let f = interactive(&mut ui, &mut b, start());
    assert_eq!(f.code, encode_open(false, 8080), "the parent opens the admin page");
    assert_eq!(f.error, None);
    assert_eq!(b.calls, ["detect", r"plan Install C:\OmniIngest account=- pw=-", r"run Install C:\OmniIngest account=- pw=-"]);
    assert_eq!(ui.shown, ["welcome", "account", "summary", "progress", "finished"]);
    assert_eq!(ui.welcomes[0].dir, PathBuf::from(r"C:\OmniIngest"));
    assert_eq!(ui.accounts[0].selected, AccountSel::BuiltIn);
    assert!(ui.accounts[0].back && ui.accounts[0].current.is_none());
    assert_eq!(ui.summaries[0].kind, SummaryKind::Install);
    assert_eq!(ui.finished, [FinishedPage::Installed { admin_url: Some("http://127.0.0.1:8080/admin".into()), kept_database: false }]);
}

#[test]
fn a_folder_chosen_on_the_welcome_page_is_the_one_installed_into() {
    let mut ui = Scripted {
        welcome: [WelcomeChoice::ChangeFolder, WelcomeChoice::ChangeFolder, WelcomeChoice::ChangeFolder, WelcomeChoice::Install].into(),
        folders: [Some(PathBuf::from(r"D:\Media\Omni")), None, Some(PathBuf::from(r"F:\"))].into(),
        account: [AccountAnswer::BuiltIn].into(),
        summary: [Nav::Confirm].into(),
        ..Default::default()
    };
    let mut b = Recorder::fresh();
    b.runs.push_back(open_admin());
    interactive(&mut ui, &mut b, start());
    let dirs: Vec<&Path> = ui.welcomes.iter().map(|w| w.dir.as_path()).collect();
    // A cancelled picker keeps the folder; a drive root gets a folder of its own.
    assert_eq!(dirs, [Path::new(r"C:\OmniIngest"), Path::new(r"D:\Media\Omni"), Path::new(r"D:\Media\Omni"), Path::new(r"F:\OmniIngest")]);
    assert_eq!(b.machine_calls(), [r"run Install F:\OmniIngest account=- pw=-"]);
}

/// The account-refusal rule: a password Windows does not take sends the
/// operator back to the Account page with the reason, not to the end.
#[test]
fn a_refused_password_returns_to_the_account_page_and_the_built_in_account_then_installs() {
    let mut ui = Scripted {
        welcome: [WelcomeChoice::Install].into(),
        account: [AccountAnswer::Windows, AccountAnswer::BuiltIn].into(),
        creds: [Some(r"NEWSROOM\svc_test")].into(),
        summary: [Nav::Confirm, Nav::Confirm].into(),
        ..Default::default()
    };
    let mut b = Recorder::fresh();
    let refusal = r"Windows did not accept the password given for NEWSROOM\svc_test; check it and run Setup again. Nothing else was changed.";
    b.runs.push_back(Outcome::Refused { notice: Vec::new(), message: refusal.into(), account: true });
    b.runs.push_back(open_admin());
    let f = interactive(&mut ui, &mut b, start());
    assert_eq!(f.code, encode_open(false, 8080));
    assert_eq!(ui.shown, ["welcome", "account", "credentials", "summary", "progress", "account", "summary", "progress", "finished"]);
    assert_eq!(ui.accounts[1].error.as_deref(), Some(refusal));
    assert_eq!(ui.accounts[1].selected, AccountSel::Windows, "the choice the operator made is still selected");
    assert_eq!(
        b.calls,
        [
            "detect",
            r"plan Install C:\OmniIngest account=NEWSROOM\svc_test pw=given",
            r"run Install C:\OmniIngest account=NEWSROOM\svc_test pw=given",
            r"plan Install C:\OmniIngest account=- pw=-",
            r"run Install C:\OmniIngest account=- pw=-",
        ]
    );
    assert!(!format!("{:?}{:?}", ui.accounts, ui.summaries).contains(PASSWORD));
}

#[test]
fn an_unknown_account_at_planning_returns_to_the_account_page_and_a_cancelled_prompt_too() {
    let mut ui = Scripted {
        welcome: [WelcomeChoice::Install].into(),
        account: [AccountAnswer::Windows, AccountAnswer::Windows, AccountAnswer::Windows, AccountAnswer::Cancel].into(),
        creds: [None, Some(r"NEWSROOM\svc_tset"), Some(r"NEWSROOM\svc_test")].into(),
        summary: [Nav::Back].into(),
        ..Default::default()
    };
    let mut b = Recorder::fresh();
    b.plans.push_back(Err(Outcome::Refused { notice: Vec::new(), message: r"Windows does not know the account NEWSROOM\svc_tset.".into(), account: true }));
    let f = interactive(&mut ui, &mut b, start());
    assert_eq!(
        ui.shown,
        ["welcome", "account", "credentials", "account", "credentials", "account", "credentials", "summary", "account"]
    );
    assert_eq!(ui.accounts[2].error.as_deref(), Some(r"Windows does not know the account NEWSROOM\svc_tset."));
    assert_eq!(ui.accounts[3].error, None, "the reason is shown once");
    assert_eq!(f.code, ExitCode::Cancelled.code());
    assert!(b.machine_calls().is_empty());
}

#[test]
fn an_upgrade_keeps_the_account_and_says_the_new_version() {
    let mut ui = Scripted { welcome: [WelcomeChoice::Upgrade].into(), summary: [Nav::Confirm].into(), ..Default::default() };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Done(Done::Ok));
    let f = interactive(&mut ui, &mut b, start());
    assert_eq!(f.code, 0);
    assert_eq!(ui.shown, ["welcome", "summary", "progress", "finished"], "no Account page: the account is kept");
    assert_eq!(b.machine_calls(), [r"run Install E:\Omni account=- pw=-"], "the installation's own folder");
    assert!(ui.welcomes[0].can_upgrade && ui.welcomes[0].installed.is_some());
    assert_eq!(ui.summaries[0].kind, SummaryKind::Upgrade);
    assert!(ui.summaries[0].account_kept);
    assert_eq!(ui.finished, [FinishedPage::Upgraded { version: VERSION.into() }]);
}

#[test]
fn an_account_change_registers_the_built_in_account_by_its_name() {
    let mut ui = Scripted { account: [AccountAnswer::BuiltIn].into(), summary: [Nav::Confirm].into(), ..Default::default() };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Done(Done::Ok));
    let s = Start::from_mode(&Mode::ChangeAccount { account: None, no_firewall: false });
    let f = interactive(&mut ui, &mut b, s);
    assert_eq!(f.code, 0);
    assert_eq!(ui.shown, ["account", "summary", "progress", "finished"], "Apps & Features' Modify goes straight to the account");
    assert_eq!(ui.accounts[0].current.as_deref(), Some(DEFAULT));
    assert!(!ui.accounts[0].back);
    assert_eq!(b.machine_calls(), [&format!(r"run ChangeAccount E:\Omni account={DEFAULT} pw=-")]);
    assert_eq!(ui.finished, [FinishedPage::AccountChanged { account: DEFAULT.into() }]);
}

#[test]
fn the_command_line_pre_fills_the_account_page() {
    // A named account: the Windows choice, its name in the prompt.
    let a = Accounts::new(Some(r"NEWSROOM\svc_test"), Some(DEFAULT));
    assert_eq!((a.sel, a.user.as_deref(), a.given.as_deref()), (AccountSel::Windows, Some(r"NEWSROOM\svc_test"), None));
    // The service's own named account, for an account change.
    let a = Accounts::new(None, Some(r".\svc_test"));
    assert_eq!((a.sel, a.user.as_deref()), (AccountSel::Windows, Some(r".\svc_test")));
    // LocalSystem or another passwordless account: offered as given.
    let a = Accounts::new(Some("LocalSystem"), None);
    assert_eq!((a.sel, a.given.as_deref()), (AccountSel::Given, Some("LocalSystem")));
    let a = Accounts::new(Some(r"NT AUTHORITY\NetworkService"), None);
    assert_eq!((a.sel, a.given.as_deref()), (AccountSel::Given, Some(r"NT AUTHORITY\NetworkService")));
    // The built-in account by name, or nothing: the built-in choice.
    for given in [Some(DEFAULT), None] {
        let a = Accounts::new(given, Some(DEFAULT));
        assert_eq!((a.sel, a.given.as_deref()), (AccountSel::BuiltIn, None));
    }
    // An upgrade with /account goes through the Account page.
    let mut ui = Scripted {
        welcome: [WelcomeChoice::Upgrade].into(),
        account: [AccountAnswer::Given].into(),
        summary: [Nav::Confirm].into(),
        ..Default::default()
    };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Done(Done::Ok));
    let mut s = start();
    s.account = Some("LocalSystem".into());
    interactive(&mut ui, &mut b, s);
    assert_eq!(b.machine_calls(), [r"run Install E:\Omni account=LocalSystem pw=-"]);
}

#[test]
fn removing_without_and_with_the_data() {
    let removal = |answers: Vec<RemoveAnswer>, remove_data: bool| {
        let mut ui = Scripted { remove: answers.into(), ..Default::default() };
        let mut b = Recorder::installed();
        b.runs.push_back(Outcome::Removed("✓ OmniIngest was removed.".into()));
        let s = Start::from_mode(&Mode::Uninstall { dir: None, remove_data });
        let f = interactive(&mut ui, &mut b, s);
        (f, ui, b)
    };
    // Keeping the data: one confirmation.
    let (f, ui, b) = removal(vec![RemoveAnswer::Remove { remove_data: false }], false);
    assert_eq!(f.code, 0);
    assert_eq!(ui.shown, ["remove", "progress", "finished"]);
    assert!(!ui.removes[0].remove_data);
    assert_eq!(b.calls, ["detect", "plan_removal None false", "run_removal None false"]);
    assert_eq!(ui.finished, [FinishedPage::Removed { sentence: "✓ OmniIngest was removed.".into() }]);
    // `/remove-data` starts on the data page; Remove there runs it.
    let (_, ui, b) = removal(vec![RemoveAnswer::Remove { remove_data: true }], true);
    assert!(ui.removes[0].remove_data);
    assert_eq!(ui.removes[0].lines, ["  plan for remove_data=true"]);
    assert_eq!(b.calls, ["detect", "plan_removal None true", "run_removal None true"]);
    // `/uninstall /dir` names the folder.
    let mut ui = Scripted { remove: [RemoveAnswer::Remove { remove_data: false }].into(), ..Default::default() };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Removed("✓ OmniIngest was removed.".into()));
    interactive(&mut ui, &mut b, Start::from_mode(&Mode::Uninstall { dir: Some(PathBuf::from(r"E:\Omni")), remove_data: false }));
    assert_eq!(b.machine_calls(), [r#"run_removal Some("E:\\Omni") false"#]);
}

/// Cancel on any page before the confirmation is exit 6 and no machine call.
#[test]
fn cancel_on_any_page_before_the_confirmation_changes_nothing() {
    let cases: Vec<(&str, Scripted, Recorder, Start)> = vec![
        ("welcome", Scripted { welcome: [WelcomeChoice::Cancel].into(), ..Default::default() }, Recorder::fresh(), start()),
        (
            "folder picker, then welcome",
            Scripted { welcome: [WelcomeChoice::ChangeFolder, WelcomeChoice::Cancel].into(), folders: [None].into(), ..Default::default() },
            Recorder::fresh(),
            start(),
        ),
        (
            "account",
            Scripted { welcome: [WelcomeChoice::Install].into(), account: [AccountAnswer::Cancel].into(), ..Default::default() },
            Recorder::fresh(),
            start(),
        ),
        (
            "credential prompt, then account",
            Scripted {
                welcome: [WelcomeChoice::Install].into(),
                account: [AccountAnswer::Windows, AccountAnswer::Cancel].into(),
                creds: [None].into(),
                ..Default::default()
            },
            Recorder::fresh(),
            start(),
        ),
        (
            "summary",
            Scripted {
                welcome: [WelcomeChoice::Install].into(),
                account: [AccountAnswer::BuiltIn].into(),
                summary: [Nav::Cancel].into(),
                ..Default::default()
            },
            Recorder::fresh(),
            start(),
        ),
        (
            "summary after back",
            Scripted {
                welcome: [WelcomeChoice::Install, WelcomeChoice::Install].into(),
                account: [AccountAnswer::BuiltIn, AccountAnswer::Back].into(),
                summary: [Nav::Back].into(),
                ..Default::default()
            },
            Recorder::fresh(),
            start(),
        ),
        (
            "upgrade summary",
            Scripted { welcome: [WelcomeChoice::Upgrade].into(), summary: [Nav::Cancel].into(), ..Default::default() },
            Recorder::installed(),
            start(),
        ),
        (
            "change account",
            Scripted { account: [AccountAnswer::Cancel].into(), ..Default::default() },
            Recorder::installed(),
            Start::from_mode(&Mode::ChangeAccount { account: None, no_firewall: false }),
        ),
        (
            "remove",
            Scripted { remove: [RemoveAnswer::Cancel].into(), ..Default::default() },
            Recorder::installed(),
            Start::from_mode(&Mode::Uninstall { dir: None, remove_data: true }),
        ),
    ];
    for (name, mut ui, mut b, s) in cases {
        // "summary after back" ends on the second welcome page's Install: cancel the account page it leads to.
        if name == "summary after back" {
            ui.account.push_back(AccountAnswer::Cancel);
        }
        let f = interactive(&mut ui, &mut b, s);
        assert_eq!(f.code, ExitCode::Cancelled.code(), "{name}");
        assert!(b.machine_calls().is_empty(), "{name}: {:?}", b.calls);
        assert!(ui.finished.is_empty(), "{name}: a cancel shows no Finished page");
    }
}

#[test]
fn a_folder_refusal_ends_on_the_refused_page_with_exit_5_and_no_machine_call() {
    let mut ui = Scripted { welcome: [WelcomeChoice::Install].into(), account: [AccountAnswer::BuiltIn].into(), ..Default::default() };
    let mut b = Recorder::fresh();
    let message = r"C:\OmniIngest exists and belongs to NEWSROOM\someone, not to Administrators.";
    b.plans.push_back(Err(Outcome::Refused { notice: vec!["  a notice".into()], message: message.into(), account: false }));
    let f = interactive(&mut ui, &mut b, start());
    assert_eq!(f.code, ExitCode::Refused.code());
    assert_eq!(f.error.as_deref(), Some(message));
    assert_eq!(ui.shown, ["welcome", "account", "finished"]);
    assert_eq!(ui.finished, [FinishedPage::Refused { notice: vec!["  a notice".into()], message: message.into() }]);
    assert!(b.machine_calls().is_empty());
}

#[test]
fn a_failure_ends_on_the_failed_page_with_the_log_and_exit_1() {
    let mut ui = Scripted {
        welcome: [WelcomeChoice::Install].into(),
        account: [AccountAnswer::BuiltIn].into(),
        summary: [Nav::Confirm].into(),
        ..Default::default()
    };
    let mut b = Recorder::fresh();
    let log = PathBuf::from(r"C:\OmniIngest\setup.log");
    b.runs.push_back(Outcome::Failed { code: ExitCode::Failed, message: "The service would not start".into(), log: Some(log.clone()) });
    let f = interactive(&mut ui, &mut b, start());
    assert_eq!(f.code, 1);
    assert_eq!(ui.finished, [FinishedPage::Failed { message: "The service would not start".into(), log: Some(log) }]);
}

#[test]
fn a_rolled_back_upgrade_shows_the_installer_s_sentence_and_exits_by_whether_it_runs() {
    let report = InstallReport {
        kind: InstallKind::Upgrade { had_previous_exe: true },
        rolled_back: true,
        failure: Some("The service did not answer on 127.0.0.1:8090 within 240 s".into()),
        restored_running: true,
        way_back: None,
        admin_url: "http://127.0.0.1:8090/admin".into(),
        health: Vec::new(),
        logs: PathBuf::from(r"E:\Omni\logs"),
        existing_database: true,
    };
    for (running, code) in [(true, ExitCode::RolledBack), (false, ExitCode::Failed)] {
        let report = InstallReport { restored_running: running, ..report.clone() };
        let message = rolled_back_message(&report, false);
        let mut ui = Scripted { welcome: [WelcomeChoice::Upgrade].into(), summary: [Nav::Confirm].into(), ..Default::default() };
        let mut b = Recorder::installed();
        b.runs.push_back(Outcome::RolledBack { message: message.clone(), running });
        let f = interactive(&mut ui, &mut b, start());
        assert_eq!(f.code, code.code(), "running={running}");
        assert_eq!(ui.finished, [FinishedPage::RolledBack { message: message.clone(), running }]);
        let shown = crate::ui::render_finished(&ui.finished[0], Lang::En, VERSION);
        assert!(shown.content.contains(&message), "{shown:?}");
    }
}

/// What `/auto` does (`TaskDialogUi` with `auto`): every page takes its
/// default button and radio button; after the skipped credential prompt every
/// page answers Cancel.
#[derive(Default)]
struct Defaults {
    finished: Vec<FinishedPage>,
    abandoned: bool,
    shown: Vec<&'static str>,
    summaries: Vec<SummaryPage>,
}

impl Defaults {
    fn button(&self, default: i32) -> i32 {
        if self.abandoned {
            crate::ui::ID_CANCEL
        } else {
            default
        }
    }
}

impl Ui for Defaults {
    fn welcome(&mut self, page: &WelcomePage) -> WelcomeChoice {
        self.shown.push("welcome");
        welcome_choice(self.button(render_welcome(page, Lang::El).default_button))
    }
    fn pick_folder(&mut self, _current: &Path) -> Option<PathBuf> {
        None
    }
    fn account(&mut self, page: &AccountPage) -> AccountAnswer {
        self.shown.push("account");
        let d = render_account(page, Lang::El, VERSION);
        account_answer(self.button(d.default_button), d.default_radio)
    }
    fn credentials(&mut self, _request: &CredRequest) -> Option<Credentials> {
        self.shown.push("credentials");
        self.abandoned = true;
        None
    }
    fn summary(&mut self, page: &SummaryPage) -> Nav {
        self.shown.push("summary");
        self.summaries.push(page.clone());
        nav(self.button(render_summary(page, Lang::El).default_button))
    }
    fn progress(&mut self, _page: &ProgressPage, job: Job<'_>) -> Outcome {
        self.shown.push("progress");
        job(&Feed::default())
    }
    fn remove(&mut self, page: &RemovePage) -> RemoveAnswer {
        let d = render_remove(page, Lang::El, VERSION);
        remove_answer(self.button(d.default_button), d.verification.is_some_and(|v| v.1))
    }
    fn finished(&mut self, page: &FinishedPage) {
        self.finished.push(page.clone());
    }
}

/// `/pages-demo <scenario> /auto` walks to the end every scenario promises.
#[test]
fn every_demo_scenario_walks_to_its_end_on_the_default_buttons() {
    for scenario in [
        Scenario::Fresh,
        Scenario::Upgrade,
        Scenario::Account,
        Scenario::Remove,
        Scenario::Refused,
        Scenario::Failed,
        Scenario::RolledBack,
        Scenario::Share,
    ] {
        let mut ui = Defaults::default();
        let mut demo = DemoBackend { scenario, pause: Duration::ZERO };
        let f = interactive(&mut ui, &mut demo, scenario.start());
        assert_eq!(f.code, scenario.expected_code(), "{scenario:?}");
        if scenario == Scenario::Share {
            assert!(ui.finished.is_empty() && !ui.shown.contains(&"progress"), "nothing runs: {:?}", ui.shown);
            assert!(!ui.summaries[0].share.is_empty(), "the demo shows the share question");
        } else {
            assert_eq!(ui.finished.len(), 1, "{scenario:?}");
        }
    }
}

// ---- the share question (P13.1c round 2) ----------------------------------------

const SHARE: &str = r"! The watchfolder \\nas-test\ingest is a network share; the built-in account reaches it as this computer.";

fn change_account() -> Start {
    Start::from_mode(&Mode::ChangeAccount { account: None, no_firewall: false })
}

/// Enter and `/auto` never answer the share question yes: the default goes
/// Back, and nothing runs.
#[test]
fn the_share_question_s_default_answer_goes_back_and_runs_nothing() {
    let mut ui = Defaults::default();
    let mut b = Recorder::installed();
    b.share = vec![SHARE.into()];
    let f = interactive(&mut ui, &mut b, change_account());
    assert_eq!(ui.shown, ["account", "summary", "account", "credentials", "account"], "Back, then a Windows account is offered");
    assert_eq!(f.code, ExitCode::Cancelled.code());
    assert!(b.machine_calls().is_empty(), "{:?}", b.calls);
    // What that Summary looks like: the question first, a warning, Back as the default.
    let d = render_summary(&ui.summaries[0], Lang::El);
    assert_eq!(d.default_button, crate::ui::ID_BACK);
    assert_eq!(d.icon, crate::ui::Icon::Warning);
    assert!(d.buttons.iter().any(|(id, _)| *id == crate::ui::ID_CONFIRM), "the affirmative button is still there");
    let lines: Vec<&str> = d.content.lines().collect();
    assert_eq!(lines[..3], [crate::texts::T::ShareHeading.el(), SHARE, crate::texts::T::ShareAdvice.el()]);
}

#[test]
fn the_affirmative_button_answers_the_share_question_yes() {
    let mut ui = Scripted { account: [AccountAnswer::BuiltIn].into(), summary: [Nav::Confirm].into(), ..Default::default() };
    let mut b = Recorder::installed();
    b.share = vec![SHARE.into()];
    b.runs.push_back(Outcome::Done(Done::Ok));
    let f = interactive(&mut ui, &mut b, change_account());
    assert_eq!(f.code, 0);
    assert_eq!(ui.summaries[0].share, [SHARE]);
    assert_eq!(b.machine_calls(), [&format!(r"run ChangeAccount E:\Omni account={DEFAULT} pw=- confirm_unc")]);
}

#[test]
fn back_from_the_share_question_lands_on_the_account_page_for_a_change_and_an_upgrade() {
    // An account change.
    let mut ui = Scripted { account: [AccountAnswer::BuiltIn, AccountAnswer::Cancel].into(), summary: [Nav::Back].into(), ..Default::default() };
    let mut b = Recorder::installed();
    b.share = vec![SHARE.into()];
    interactive(&mut ui, &mut b, change_account());
    assert_eq!(ui.shown, ["account", "summary", "account"]);
    assert_eq!(ui.accounts[1].selected, AccountSel::Windows);
    // An upgrade, which has no Account page in its own path: Back reaches it,
    // and a Windows account chosen there is the one the upgrade runs with.
    let mut ui = Scripted {
        welcome: [WelcomeChoice::Upgrade].into(),
        summary: [Nav::Back, Nav::Confirm].into(),
        account: [AccountAnswer::Windows].into(),
        creds: [Some(r"NEWSROOM\svc_test")].into(),
        ..Default::default()
    };
    let mut b = Recorder::installed();
    b.share = vec![SHARE.into()];
    b.runs.push_back(Outcome::Done(Done::Ok));
    interactive(&mut ui, &mut b, start());
    assert_eq!(ui.shown, ["welcome", "summary", "account", "credentials", "summary", "progress", "finished"]);
    assert_eq!(ui.summaries[1].share, Vec::<String>::new(), "a Windows account asks no share question");
    assert_eq!(b.machine_calls(), [r"run Install E:\Omni account=NEWSROOM\svc_test pw=given"]);
}

#[test]
fn without_the_share_question_the_summary_s_default_is_the_affirmative_button() {
    let mut ui = Defaults::default();
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Done(Done::Ok));
    let f = interactive(&mut ui, &mut b, change_account());
    assert_eq!(f.code, 0);
    assert_eq!(render_summary(&ui.summaries[0], Lang::En).default_button, crate::ui::ID_CONFIRM);
    assert_eq!(b.machine_calls(), [&format!(r"run ChangeAccount E:\Omni account={DEFAULT} pw=-")], "no confirm_unc without the question");
}

// ---- round 3 --------------------------------------------------------------------

fn uninstall(remove_data: bool) -> Start {
    Start::from_mode(&Mode::Uninstall { dir: None, remove_data })
}

/// Y1: the checkbox ticked on the keep page shows the page again, drawn from
/// the plan that removes the data; only Remove on that page runs it.
#[test]
fn ticking_the_checkbox_shows_the_data_page_before_anything_is_removed() {
    let mut ui = Scripted {
        remove: [RemoveAnswer::Remove { remove_data: true }, RemoveAnswer::Remove { remove_data: true }].into(),
        ..Default::default()
    };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Removed("✓ OmniIngest was removed.".into()));
    let f = interactive(&mut ui, &mut b, uninstall(false));
    assert_eq!(f.code, 0);
    assert_eq!(ui.shown, ["remove", "remove", "progress", "finished"]);
    assert_eq!((ui.removes[0].remove_data, ui.removes[0].lines.as_slice()), (false, &["  plan for remove_data=false".to_string()][..]));
    assert_eq!((ui.removes[1].remove_data, ui.removes[1].lines.as_slice()), (true, &["  plan for remove_data=true".to_string()][..]));
    assert_eq!(b.calls, ["detect", "plan_removal None false", "plan_removal None true", "run_removal None true"]);
    // Ticked, then Cancel on the data page: nothing ran.
    let mut ui = Scripted { remove: [RemoveAnswer::Remove { remove_data: true }, RemoveAnswer::Cancel].into(), ..Default::default() };
    let mut b = Recorder::installed();
    assert_eq!(interactive(&mut ui, &mut b, uninstall(false)).code, ExitCode::Cancelled.code());
    assert!(b.machine_calls().is_empty(), "{:?}", b.calls);
}

#[test]
fn unticking_on_the_data_page_shows_the_keep_page_again() {
    let mut ui = Scripted {
        remove: [RemoveAnswer::Remove { remove_data: false }, RemoveAnswer::Remove { remove_data: false }].into(),
        ..Default::default()
    };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Removed("✓ OmniIngest was removed.".into()));
    interactive(&mut ui, &mut b, uninstall(true));
    assert_eq!(ui.shown, ["remove", "remove", "progress", "finished"]);
    assert!(ui.removes[0].remove_data && !ui.removes[1].remove_data);
    assert_eq!(b.calls, ["detect", "plan_removal None true", "plan_removal None false", "run_removal None false"]);
}

/// Y1: Enter and `/auto` never delete data: the data page's default is Cancel.
#[test]
fn the_data_page_defaults_to_cancel_and_the_keep_page_to_remove() {
    let mut ui = Defaults::default();
    let mut b = Recorder::installed();
    assert_eq!(interactive(&mut ui, &mut b, uninstall(true)).code, ExitCode::Cancelled.code());
    assert!(b.machine_calls().is_empty(), "{:?}", b.calls);
    let page = |remove_data| RemovePage { lines: vec!["x".into()], remove_data };
    assert_eq!(render_remove(&page(true), Lang::El, VERSION).default_button, crate::ui::ID_CANCEL);
    assert_eq!(render_remove(&page(false), Lang::El, VERSION).default_button, crate::ui::ID_REMOVE);
    assert_eq!(render_remove(&page(true), Lang::El, VERSION).verification, Some((crate::texts::T::RemoveData.el().into(), true)));
}

/// Y2: however the pages end, the backend's work folder is cleaned up then,
/// and a line saying it could not be reaches the operator.
#[test]
fn a_work_folder_left_behind_is_told_after_the_pages_whatever_their_end() {
    let line = r"! Could not remove Setup's work folder C:\ProgramData\OmniIngest-Setup-0000000000000001 (denied); delete it by hand.";
    let cases: Vec<(Scripted, Vec<Outcome>, u32)> = vec![
        (Scripted { welcome: [WelcomeChoice::Cancel].into(), ..Default::default() }, vec![], ExitCode::Cancelled.code()),
        (
            Scripted { welcome: [WelcomeChoice::Install].into(), account: [AccountAnswer::BuiltIn].into(), summary: [Nav::Confirm].into(), ..Default::default() },
            vec![open_admin()],
            ExitCode::Ok.code(),
        ),
        (
            Scripted { welcome: [WelcomeChoice::Install].into(), account: [AccountAnswer::BuiltIn].into(), summary: [Nav::Confirm].into(), ..Default::default() },
            vec![Outcome::Failed { code: ExitCode::BadPayload, message: "damaged".into(), log: None }],
            ExitCode::BadPayload.code(),
        ),
    ];
    for (mut ui, runs, code) in cases {
        let mut b = Recorder::fresh();
        b.runs = runs.into();
        b.cleanup = Some(line.into());
        let mut told = Vec::new();
        let f = crate::run::pages_with(&mut ui, &mut b, start(), false, &mut |t: &str| told.push(t.to_string()));
        assert_eq!(f.code, code);
        assert_eq!(told, [line], "code {code}");
        assert_eq!(b.calls.last().map(String::as_str), Some("cleanup"));
    }
    // Nothing left behind: nothing told.
    let mut ui = Scripted { welcome: [WelcomeChoice::Cancel].into(), ..Default::default() };
    let mut told = Vec::new();
    crate::run::pages_with(&mut ui, &mut Recorder::fresh(), start(), false, &mut |t: &str| told.push(t.to_string()));
    assert!(told.is_empty());
}

/// Y3: the built-in account chosen on an upgrade's Account page is passed by
/// name; an upgrade that never showed the page keeps the account.
#[test]
fn an_upgrade_registers_the_built_in_account_only_when_it_was_chosen() {
    let mut ui = Scripted {
        welcome: [WelcomeChoice::Upgrade].into(),
        account: [AccountAnswer::BuiltIn].into(),
        summary: [Nav::Confirm].into(),
        ..Default::default()
    };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Done(Done::Ok));
    let mut s = start();
    s.account = Some("LocalSystem".into());
    interactive(&mut ui, &mut b, s);
    assert_eq!(ui.shown[..3], ["welcome", "account", "summary"]);
    assert_eq!(b.machine_calls(), [&format!(r"run Install E:\Omni account={DEFAULT} pw=-")]);
    let mut ui = Scripted { welcome: [WelcomeChoice::Upgrade].into(), summary: [Nav::Confirm].into(), ..Default::default() };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Done(Done::Ok));
    interactive(&mut ui, &mut b, start());
    assert_eq!(b.machine_calls(), [r"run Install E:\Omni account=- pw=-"], "kept: no Account page was shown");
}

/// P13.1d: an account picked on the change-account path and backed out of to
/// the Welcome page is not carried into a later upgrade, which keeps the account.
#[test]
fn an_account_backed_out_of_to_the_welcome_page_is_not_carried_into_an_upgrade() {
    let mut ui = Scripted {
        welcome: [WelcomeChoice::ChangeAccount, WelcomeChoice::Upgrade].into(),
        account: [AccountAnswer::BuiltIn, AccountAnswer::Back].into(),
        summary: [Nav::Back, Nav::Confirm].into(),
        ..Default::default()
    };
    let mut b = Recorder::installed();
    b.runs.push_back(Outcome::Done(Done::Ok));
    let f = interactive(&mut ui, &mut b, start());
    assert_eq!(f.code, 0);
    assert_eq!(ui.shown, ["welcome", "account", "summary", "account", "welcome", "summary", "progress", "finished"]);
    assert_eq!(ui.summaries[1].kind, SummaryKind::Upgrade);
    assert!(ui.summaries[1].account_kept, "the upgrade's summary says the account is kept");
    assert_eq!(b.machine_calls(), [r"run Install E:\Omni account=- pw=-"], "the upgrade keeps the account");
}

/// Y6: a new installation over a kept database is not told to use Admin / Admin.
#[test]
fn a_new_installation_over_a_kept_database_says_to_sign_in_as_its_administrator() {
    for (existing, kept_key) in [(false, crate::texts::T::FinInstalledContent), (true, crate::texts::T::FinInstalledKeptContent)] {
        let mut ui = Scripted {
            welcome: [WelcomeChoice::Install].into(),
            account: [AccountAnswer::BuiltIn].into(),
            summary: [Nav::Confirm].into(),
            ..Default::default()
        };
        let mut b = Recorder::fresh();
        b.existing_database = existing;
        b.runs.push_back(open_admin());
        interactive(&mut ui, &mut b, start());
        assert_eq!(ui.finished, [FinishedPage::Installed { admin_url: Some("http://127.0.0.1:8080/admin".into()), kept_database: existing }]);
        for lang in [Lang::En, Lang::El] {
            let d = crate::ui::render_finished(&ui.finished[0], lang, VERSION);
            assert!(d.content.starts_with(crate::texts::tr(lang, kept_key)), "{lang:?} {existing}: {}", d.content);
            assert_eq!(d.content.contains("Admin / Admin"), !existing, "{lang:?} {existing}");
        }
    }
}
