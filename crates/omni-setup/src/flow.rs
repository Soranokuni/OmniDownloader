//! The order of Setup's pages (P13.1c), as a pure function over a [`Ui`] and
//! a [`Backend`]: Welcome → Account → Summary → Progress → Finished, or
//! Remove → Progress → Finished. Nothing changes the machine before the
//! operator confirms the summary (or the Remove page); Cancel before that
//! is exit 6.

use std::path::PathBuf;

use omni_service::{default_or_given, AccountChoice, DEFAULT_SERVICE_ACCOUNT};

use crate::args::Mode;
use crate::backend::{Backend, Installed, Op, OpKind, Outcome, PlanSummary};
use crate::exit::{admin_url, ExitCode};
use crate::run::{Done, Finished, VERSION};
use crate::ui::{
    AccountAnswer, AccountPage, AccountSel, CredRequest, FinishedPage, Nav, ProgressPage, RemoveAnswer, RemovePage, Secret,
    SummaryKind, SummaryPage, Ui, WelcomeChoice, WelcomePage, Work,
};

/// The first page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Welcome,
    /// `/change-account` (Apps & Features' Modify button).
    ChangeAccount,
    /// `/uninstall` (Apps & Features' Uninstall button).
    Uninstall,
}

/// What the command line gave the pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub entry: Entry,
    /// The folder of a new installation (`/dir`, or the default).
    pub dir: PathBuf,
    /// `/dir` with `/uninstall`.
    pub uninstall_dir: Option<PathBuf>,
    /// `/account`: pre-fills the Account page.
    pub account: Option<String>,
    /// `/remove-data`: pre-checks the Remove page's checkbox.
    pub remove_data: bool,
}

impl Start {
    pub fn from_mode(mode: &Mode) -> Start {
        let default_dir = PathBuf::from(omni_cli::install::DEFAULT_DIR);
        match mode {
            Mode::Install { dir, account, .. } => {
                Start { entry: Entry::Welcome, dir: dir.clone(), uninstall_dir: None, account: account.clone(), remove_data: false }
            }
            Mode::ChangeAccount { account, .. } => {
                Start { entry: Entry::ChangeAccount, dir: default_dir, uninstall_dir: None, account: account.clone(), remove_data: false }
            }
            Mode::Uninstall { dir, remove_data } => Start {
                entry: Entry::Uninstall,
                dir: dir.clone().unwrap_or(default_dir),
                uninstall_dir: dir.clone(),
                account: None,
                remove_data: *remove_data,
            },
        }
    }
}

/// Where the account pages lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Install,
    Upgrade,
    Change,
}

enum Page {
    Welcome,
    Account(Target),
    Summary(Target),
    Remove,
}

/// The account chosen on the Account page.
enum Pick {
    BuiltIn,
    Given(String),
    Windows { user: String, password: Secret },
}

/// The state of the Account page between visits.
struct Accounts {
    pick: Pick,
    sel: AccountSel,
    given: Option<String>,
    /// The user name the credential prompt starts with.
    user: Option<String>,
    error: Option<String>,
    /// The Account page was used (an upgrade otherwise keeps the account).
    chosen: bool,
}

impl Accounts {
    /// Pre-filled from `/account`, else from the account the service runs as.
    fn new(given: Option<&str>, current: Option<&str>) -> Accounts {
        let mut a = Accounts { pick: Pick::BuiltIn, sel: AccountSel::BuiltIn, given: None, user: None, error: None, chosen: false };
        let from = given.or(current);
        match from.map(|s| default_or_given(Some(s))) {
            Some(AccountChoice::NeedsPassword(name)) => {
                a.sel = AccountSel::Windows;
                a.user = Some(name);
            }
            Some(AccountChoice::Passwordless(name)) if given.is_some() && !name.eq_ignore_ascii_case(DEFAULT_SERVICE_ACCOUNT) => {
                a.sel = AccountSel::Given;
                a.pick = Pick::Given(name.clone());
                a.given = Some(name);
            }
            Some(AccountChoice::LocalSystem) if given.is_some() => {
                let name = given.unwrap_or_default().trim().to_string();
                a.sel = AccountSel::Given;
                a.pick = Pick::Given(name.clone());
                a.given = Some(name);
            }
            _ => {}
        }
        a
    }

    /// The account and password of an operation of `target`.
    fn op(&self, target: Target) -> (Option<String>, Option<&str>) {
        match (&self.pick, target) {
            // Only an upgrade that never showed the Account page keeps the account.
            (_, Target::Upgrade) if !self.chosen => (None, None),
            // Chosen on the page: the built-in account by its name (Y3).
            (Pick::BuiltIn, Target::Change | Target::Upgrade) => (Some(DEFAULT_SERVICE_ACCOUNT.into()), None),
            (Pick::BuiltIn, Target::Install) => (None, None),
            (Pick::Given(name), _) => (Some(name.clone()), None),
            (Pick::Windows { user, password }, _) => (Some(user.clone()), Some(password.expose())),
        }
    }
}

fn cancelled() -> Finished {
    Finished { code: ExitCode::Cancelled.code(), error: Some("Nothing was changed.".into()) }
}

/// Show the Finished page for `outcome` and end with its exit code.
fn finish(ui: &mut dyn Ui, summary: Option<&PlanSummary>, outcome: Outcome) -> Finished {
    let page = match &outcome {
        Outcome::Done(done) => match summary.map(|s| s.kind) {
            Some(SummaryKind::Upgrade) => FinishedPage::Upgraded { version: VERSION.into() },
            Some(SummaryKind::ChangeAccount) => FinishedPage::AccountChanged {
                account: summary.map_or(DEFAULT_SERVICE_ACCOUNT, |s| s.account.as_str()).into(),
            },
            _ => FinishedPage::Installed {
                admin_url: match done {
                    Done::OpenAdmin { tls, port } => Some(admin_url(*tls, *port)),
                    Done::Ok => None,
                },
                kept_database: summary.is_some_and(|s| s.existing_database),
            },
        },
        Outcome::Removed(sentence) => FinishedPage::Removed { sentence: sentence.clone() },
        Outcome::RolledBack { message, running } => FinishedPage::RolledBack { message: message.clone(), running: *running },
        Outcome::Failed { message, log, .. } => FinishedPage::Failed { message: message.clone(), log: log.clone() },
        Outcome::Refused { notice, message, .. } => FinishedPage::Refused { notice: notice.clone(), message: message.clone() },
    };
    ui.finished(&page);
    Finished { code: outcome.code(), error: outcome.error() }
}

fn work(kind: SummaryKind) -> Work {
    match kind {
        SummaryKind::Install => Work::Install,
        SummaryKind::Upgrade => Work::Upgrade,
        SummaryKind::ChangeAccount => Work::ChangeAccount,
    }
}

/// Setup's pages from `start` to the end, over `backend`.
pub fn interactive(ui: &mut dyn Ui, backend: &mut dyn Backend, start: Start) -> Finished {
    let detected = backend.detect();
    let installed: Option<Installed> = detected.installed.clone();
    let mut dir = start.dir.clone();
    let mut accounts = Accounts::new(start.account.as_deref(), installed.as_ref().and_then(|i| i.account.as_deref()));
    let mut remove_data = start.remove_data;
    let mut page = match start.entry {
        Entry::Welcome => Page::Welcome,
        Entry::ChangeAccount => Page::Account(Target::Change),
        Entry::Uninstall => Page::Remove,
    };
    loop {
        page = match page {
            Page::Welcome => {
                let welcome = WelcomePage {
                    version: VERSION.into(),
                    dir: dir.clone(),
                    installed: installed.clone(),
                    can_upgrade: detected.has_payload,
                };
                match ui.welcome(&welcome) {
                    WelcomeChoice::Install => Page::Account(Target::Install),
                    WelcomeChoice::ChangeFolder => {
                        if let Some(d) = ui.pick_folder(&dir) {
                            // A drive root is never the installation itself.
                            dir = if d.parent().is_none() { d.join("OmniIngest") } else { d };
                        }
                        Page::Welcome
                    }
                    // An upgrade keeps the account; it asks for one only when `/account` named it.
                    WelcomeChoice::Upgrade if start.account.is_some() => Page::Account(Target::Upgrade),
                    WelcomeChoice::Upgrade => Page::Summary(Target::Upgrade),
                    WelcomeChoice::ChangeAccount => Page::Account(Target::Change),
                    WelcomeChoice::Remove => Page::Remove,
                    WelcomeChoice::Cancel => return cancelled(),
                }
            }
            Page::Account(target) => {
                let account_page = AccountPage {
                    current: (target == Target::Change).then(|| installed.as_ref().and_then(|i| i.account.clone())).flatten(),
                    given: accounts.given.clone(),
                    selected: accounts.sel,
                    error: accounts.error.take(),
                    back: start.entry == Entry::Welcome,
                };
                match ui.account(&account_page) {
                    AccountAnswer::BuiltIn => {
                        accounts.sel = AccountSel::BuiltIn;
                        accounts.pick = Pick::BuiltIn;
                        accounts.chosen = true;
                        Page::Summary(target)
                    }
                    AccountAnswer::Given => {
                        accounts.sel = AccountSel::Given;
                        accounts.pick = Pick::Given(accounts.given.clone().unwrap_or_default());
                        accounts.chosen = true;
                        Page::Summary(target)
                    }
                    AccountAnswer::Windows => {
                        accounts.sel = AccountSel::Windows;
                        match ui.credentials(&CredRequest { user: accounts.user.clone() }) {
                            Some(c) => {
                                accounts.user = Some(c.user.clone());
                                accounts.pick = Pick::Windows { user: c.user, password: c.password };
                                accounts.chosen = true;
                                Page::Summary(target)
                            }
                            // The prompt was cancelled: the Account page again.
                            None => Page::Account(target),
                        }
                    }
                    AccountAnswer::Back => Page::Welcome,
                    AccountAnswer::Cancel => return cancelled(),
                }
            }
            Page::Summary(target) => {
                let op_dir = match target {
                    Target::Install => dir.clone(),
                    Target::Upgrade | Target::Change => installed.as_ref().map_or_else(|| dir.clone(), |i| i.dir.clone()),
                };
                let kind = if target == Target::Change { OpKind::ChangeAccount } else { OpKind::Install };
                let (account, password) = accounts.op(target);
                let op = Op { kind, dir: op_dir, account, password, confirm_unc: false };
                let summary: PlanSummary = match backend.plan(&op) {
                    Ok(s) => s,
                    Err(Outcome::Refused { message, account: true, .. }) => {
                        accounts.error = Some(message);
                        page = Page::Account(target);
                        continue;
                    }
                    Err(outcome) => return finish(ui, None, outcome),
                };
                let summary_page = SummaryPage {
                    kind: summary.kind,
                    version: VERSION.into(),
                    dir: summary.dir.clone(),
                    account: summary.account.clone(),
                    account_kept: summary.account_kept,
                    firewall: summary.firewall,
                    notes: summary.notes.clone(),
                    share: summary.share.clone(),
                };
                let asks_share = !summary.share.is_empty();
                match ui.summary(&summary_page) {
                    Nav::Confirm => {}
                    // The share question's way out: the Account page, with a
                    // Windows account offered first, for an upgrade too.
                    Nav::Back if asks_share => {
                        accounts.sel = AccountSel::Windows;
                        page = Page::Account(target);
                        continue;
                    }
                    Nav::Back => {
                        page = match target {
                            Target::Upgrade if !accounts.chosen => Page::Welcome,
                            t => Page::Account(t),
                        };
                        continue;
                    }
                    Nav::Cancel => return cancelled(),
                }
                // The affirmative button of a Summary that asked is the answer yes.
                let op = Op { confirm_unc: asks_share, ..op };
                let progress = ProgressPage { work: work(summary.kind), version: VERSION.into() };
                let outcome = ui.progress(&progress, Box::new(|feed| backend.run(&op, feed)));
                match outcome {
                    // A wrong password, a logon that is not allowed: ask again.
                    Outcome::Refused { message, account: true, .. } => {
                        accounts.error = Some(message);
                        Page::Account(target)
                    }
                    outcome => return finish(ui, Some(&summary), outcome),
                }
            }
            Page::Remove => {
                let udir = start.uninstall_dir.as_deref();
                let lines = match backend.plan_removal(udir, remove_data) {
                    Ok(l) => l,
                    Err(outcome) => return finish(ui, None, outcome),
                };
                match ui.remove(&RemovePage { lines, remove_data }) {
                    RemoveAnswer::Cancel => return cancelled(),
                    RemoveAnswer::Remove { remove_data: checked } if checked != remove_data => {
                        // The page said what the other state keeps: show it again,
                        // drawn from the plan for the checkbox as it is now
                        // (P13.1c round 3, Y1). Only a page whose text matches runs.
                        remove_data = checked;
                        Page::Remove
                    }
                    RemoveAnswer::Remove { .. } => {
                        let progress = ProgressPage { work: Work::Remove, version: VERSION.into() };
                        let outcome = ui.progress(&progress, Box::new(|feed| backend.run_removal(udir, remove_data, feed)));
                        return finish(ui, None, outcome);
                    }
                }
            }
        };
    }
}

#[cfg(test)]
#[path = "flow_tests.rs"]
mod tests;
