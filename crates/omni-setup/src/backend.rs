//! What Setup's pages ask of the machine (P13.1c), behind one trait so the
//! flow is tested without one: [`crate::real::RealBackend`] wraps what
//! `/quiet` does; [`DemoBackend`] is a harmless fake for `/pages-demo` that
//! never touches the machine.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_cli::install::{rolled_back_message, InstallKind, InstallReport, Step};

use crate::exit::{encode_open, ExitCode};
use crate::flow::{Entry, Start};
use crate::run::{uninstall_outcome, uninstall_summary, Done, Ownership};
use crate::ui::{Feed, SummaryKind};

/// An installation found on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub dir: PathBuf,
    /// The Apps & Features entry's version, when there is one.
    pub version: Option<String>,
    /// The account the registered service runs as.
    pub account: Option<String>,
    /// The service is registered.
    pub service: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub installed: Option<Installed>,
    /// This Setup carries a payload (it can install and upgrade).
    pub has_payload: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// A new installation or an upgrade: the plan tells which.
    Install,
    ChangeAccount,
}

/// What the operator chose. `account: None` takes the installer's default
/// (the built-in account for a new installation, the kept one for an upgrade).
#[derive(Clone)]
pub struct Op<'a> {
    pub kind: OpKind,
    pub dir: PathBuf,
    pub account: Option<String>,
    /// The named account's password, from the credential prompt.
    pub password: Option<&'a str>,
    /// Yes to the share question ([`PlanSummary::share`]): the operator pressed
    /// the affirmative button of a Summary that asked it.
    pub confirm_unc: bool,
}

impl fmt::Debug for Op<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Op")
            .field("kind", &self.kind)
            .field("dir", &self.dir)
            .field("account", &self.account)
            .field("password", &self.password.map(|_| "<hidden>"))
            .field("confirm_unc", &self.confirm_unc)
            .finish()
    }
}

/// What the summary page says about a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSummary {
    pub kind: SummaryKind,
    pub dir: PathBuf,
    pub account: String,
    pub account_kept: bool,
    pub firewall: Option<u16>,
    /// `Target.warnings`.
    pub notes: Vec<String>,
    /// The text of the share question (`Question::ConfirmUncWithoutAccount`:
    /// a network watchfolder and the built-in account); empty when not asked.
    /// Its default answer is No: the Summary's default button is Back.
    pub share: Vec<String>,
    /// A database is in the folder already: a new installation has no
    /// first-run Admin / Admin account (Y6).
    pub existing_database: bool,
}

/// How a planned or run piece of work ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(Done),
    /// Removed; the closing sentence.
    Removed(String),
    /// `running`: the previous state is back and runs (exit 7, else 1).
    RolledBack { message: String, running: bool },
    /// Nothing was changed (exit 5). `account`: the account was refused.
    Refused { notice: Vec<String>, message: String, account: bool },
    /// `log`: the setup.log written for this run.
    Failed { code: ExitCode, message: String, log: Option<PathBuf> },
}

impl Outcome {
    pub fn code(&self) -> u32 {
        match self {
            Outcome::Done(Done::Ok) | Outcome::Removed(_) => ExitCode::Ok.code(),
            Outcome::Done(Done::OpenAdmin { tls, port }) => encode_open(*tls, *port),
            Outcome::RolledBack { running: true, .. } => ExitCode::RolledBack.code(),
            Outcome::RolledBack { running: false, .. } => ExitCode::Failed.code(),
            Outcome::Refused { .. } => ExitCode::Refused.code(),
            Outcome::Failed { code, .. } => code.code(),
        }
    }

    /// The error text of an outcome that is not a success.
    pub fn error(&self) -> Option<String> {
        match self {
            Outcome::Done(_) | Outcome::Removed(_) => None,
            Outcome::RolledBack { message, .. } | Outcome::Refused { message, .. } | Outcome::Failed { message, .. } => {
                Some(message.clone())
            }
        }
    }
}

/// The operations the pages need. `detect` and the `plan*` methods only read
/// the machine; `run` and `run_removal` change it. `Send`: the work runs on
/// the progress page's worker thread.
pub trait Backend: Send {
    fn detect(&mut self) -> Detected;
    /// The plan of `op`, or why it cannot be done (nothing changed).
    fn plan(&mut self, op: &Op<'_>) -> Result<PlanSummary, Outcome>;
    fn run(&mut self, op: &Op<'_>, feed: &Feed) -> Outcome;
    /// What an uninstall removes and keeps, or why it is refused.
    fn plan_removal(&mut self, dir: Option<&Path>, remove_data: bool) -> Result<Vec<String>, Outcome>;
    fn run_removal(&mut self, dir: Option<&Path>, remove_data: bool, feed: &Feed) -> Outcome;
    /// Remove what the backend unpacked, once the pages have ended; a line
    /// when it could not be removed (P13.1c Y2).
    fn cleanup(&mut self) -> Option<String> {
        None
    }
}

// ---- the demo ---------------------------------------------------------------------

/// `/pages-demo`'s scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Fresh,
    Upgrade,
    Account,
    Remove,
    Refused,
    Failed,
    RolledBack,
    /// An account change whose watchfolder is a network share: the Summary
    /// asks the share question with Back as its default. With `/auto` the walk
    /// goes back, finds no Windows account (the prompt is skipped) and cancels.
    Share,
}

impl Scenario {
    pub fn parse(s: &str) -> Option<Scenario> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "fresh" => Scenario::Fresh,
            "upgrade" => Scenario::Upgrade,
            "account" => Scenario::Account,
            "remove" => Scenario::Remove,
            "refused" => Scenario::Refused,
            "failed" => Scenario::Failed,
            "rolledback" => Scenario::RolledBack,
            "share" => Scenario::Share,
            _ => return None,
        })
    }

    /// The exit code the walk ends with when every page takes its default.
    pub fn expected_code(self) -> u32 {
        match self {
            Scenario::Fresh => encode_open(false, DEMO_PORT),
            Scenario::Upgrade | Scenario::Account | Scenario::Remove => ExitCode::Ok.code(),
            Scenario::Refused => ExitCode::Refused.code(),
            Scenario::Failed => ExitCode::Failed.code(),
            Scenario::RolledBack => ExitCode::RolledBack.code(),
            // Nothing is run: the default answer to the share question is No.
            Scenario::Share => ExitCode::Cancelled.code(),
        }
    }

    fn installed(self) -> bool {
        matches!(self, Scenario::Upgrade | Scenario::Account | Scenario::Remove | Scenario::RolledBack | Scenario::Share)
    }

    /// Where the walk starts: the Account page for `account` and `share`, the Remove page for `remove`.
    pub fn start(self) -> Start {
        let entry = match self {
            Scenario::Account | Scenario::Share => Entry::ChangeAccount,
            Scenario::Remove => Entry::Uninstall,
            _ => Entry::Welcome,
        };
        Start { entry, dir: PathBuf::from(DEMO_DIR), uninstall_dir: None, account: None, remove_data: false }
    }
}

const DEMO_PORT: u16 = 8080;
const DEMO_DIR: &str = r"C:\OmniIngest";

/// Fixed data and short pauses; never touches the machine.
#[derive(Debug, Clone)]
pub struct DemoBackend {
    pub scenario: Scenario,
    /// The pause between two progress lines.
    pub pause: Duration,
}

impl DemoBackend {
    pub fn new(scenario: Scenario) -> Self {
        DemoBackend { scenario, pause: Duration::from_millis(120) }
    }

    fn feed(&self, feed: &Feed, lines: &[(Step, &str)]) {
        for (step, line) in lines {
            feed.line(*step, line);
            std::thread::sleep(self.pause);
        }
    }

    fn ownership() -> Ownership {
        Ownership { service: true, firewall: true, key: true, other: None }
    }
}

impl Backend for DemoBackend {
    fn detect(&mut self) -> Detected {
        let installed = self.scenario.installed().then(|| Installed {
            dir: PathBuf::from(DEMO_DIR),
            version: Some("0.9.4".into()),
            account: Some(omni_service::DEFAULT_SERVICE_ACCOUNT.into()),
            service: true,
        });
        Detected { installed, has_payload: true }
    }

    fn plan(&mut self, op: &Op<'_>) -> Result<PlanSummary, Outcome> {
        if self.scenario == Scenario::Refused {
            return Err(Outcome::Refused {
                notice: Vec::new(),
                message: format!(
                    r"{} exists and belongs to DEMO\someone, not to Administrators. Setup does not install into a folder another account controls. (demo)",
                    op.dir.display()
                ),
                account: false,
            });
        }
        let kind = match (op.kind, self.scenario.installed()) {
            (OpKind::ChangeAccount, _) => SummaryKind::ChangeAccount,
            (OpKind::Install, true) => SummaryKind::Upgrade,
            (OpKind::Install, false) => SummaryKind::Install,
        };
        let kept = kind == SummaryKind::Upgrade && op.account.is_none();
        let account = op.account.clone().unwrap_or_else(|| omni_service::DEFAULT_SERVICE_ACCOUNT.into());
        // The installer's own warning text, as it says it (`share_warning`).
        let share = if self.scenario == Scenario::Share && account.eq_ignore_ascii_case(omni_service::DEFAULT_SERVICE_ACCOUNT) {
            vec![
                format!(r"! The watchfolder \\nas-demo\ingest is a network share. {account} reaches it as this computer's account (demo)."),
                "  A NAS usually refuses that: give a Windows account the share admits. (demo)".into(),
            ]
        } else {
            Vec::new()
        };
        Ok(PlanSummary { kind, dir: op.dir.clone(), account, account_kept: kept, firewall: Some(DEMO_PORT), notes: Vec::new(), share, existing_database: false })
    }

    fn run(&mut self, op: &Op<'_>, feed: &Feed) -> Outcome {
        self.feed(
            feed,
            &[
                (Step::Plan, "OmniDownloader (demo) → C:\\OmniIngest"),
                (Step::Stop, "  Stopping the service… (demo)"),
                (Step::Copy, "  Copied omni-ingest.exe (demo)"),
                (Step::Config, "  Configuration written (demo)"),
                (Step::Register, "  Service registered (demo)"),
                (Step::Rights, "  Permissions set (demo)"),
                (Step::Firewall, "  Firewall: TCP 8080 open on domain and private networks. (demo)"),
                (Step::Start, "  Starting the service and waiting for it to answer… (demo)"),
            ],
        );
        match self.scenario {
            Scenario::Failed => Outcome::Failed {
                code: ExitCode::Failed,
                message: "The service would not start: the demo says so".into(),
                log: Some(PathBuf::from(DEMO_DIR).join(crate::log::SETUP_LOG)),
            },
            Scenario::RolledBack => {
                self.feed(feed, &[(Step::Rollback, "  Putting the previous state back… (demo)")]);
                let report = InstallReport {
                    kind: InstallKind::Upgrade { had_previous_exe: true },
                    rolled_back: true,
                    failure: Some("The service did not answer on 127.0.0.1:8080 within 240 s".into()),
                    restored_running: true,
                    way_back: None,
                    admin_url: format!("http://127.0.0.1:{DEMO_PORT}/admin"),
                    health: Vec::new(),
                    logs: PathBuf::from(DEMO_DIR).join("logs"),
                    existing_database: true,
                };
                Outcome::RolledBack { message: rolled_back_message(&report, op.kind == OpKind::ChangeAccount), running: true }
            }
            _ if self.scenario.installed() => Outcome::Done(Done::Ok),
            _ => Outcome::Done(Done::OpenAdmin { tls: false, port: DEMO_PORT }),
        }
    }

    fn plan_removal(&mut self, _dir: Option<&Path>, remove_data: bool) -> Result<Vec<String>, Outcome> {
        Ok(uninstall_summary(Path::new(DEMO_DIR), &[], remove_data, &Self::ownership()))
    }

    fn run_removal(&mut self, _dir: Option<&Path>, remove_data: bool, feed: &Feed) -> Outcome {
        let data = if remove_data {
            "  Removed the configuration, database and logs. (demo)"
        } else {
            "  The configuration, database and logs stay. (demo)"
        };
        self.feed(
            feed,
            &[
                (Step::Register, "✓ Service removed. (demo)"),
                (Step::Plan, "  Removed the program files. (demo)"),
                (Step::Plan, data),
            ],
        );
        Outcome::Removed(uninstall_outcome(Path::new(DEMO_DIR), &Self::ownership(), true))
    }
}
