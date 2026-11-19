//! Setup's pages as data (P13.1c): [`Ui`] has one method per page and returns
//! the operator's choice as plain data, so [`crate::flow`] is tested with a
//! scripted fake. What a page says is rendered here, purely, into a
//! [`Dialog`]; the windowed program only turns a `Dialog` into a task dialog.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use omni_cli::install::Step;

use crate::backend::{Installed, Outcome};
use crate::texts::{fmt, step_key, tr, Lang, T};

// ---- secrets -------------------------------------------------------------------

/// Overwrite `bytes` with zeros in a way the optimiser keeps.
pub fn zero_bytes(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: a valid, aligned, exclusive reference.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// [`zero_bytes`] for UTF-16 buffers handed to Win32.
pub fn zero_u16(units: &mut [u16]) {
    for u in units.iter_mut() {
        // SAFETY: a valid, aligned, exclusive reference.
        unsafe { std::ptr::write_volatile(u, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// Zero a string's bytes, then free it.
pub fn zero_string(s: String) {
    let mut bytes = s.into_bytes();
    zero_bytes(&mut bytes);
}

/// A password from the credential prompt: never printed, zeroed when dropped.
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Self {
        Secret(s)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zero_string(std::mem::take(&mut self.0));
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<hidden>")
    }
}

// ---- the pages -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomePage {
    pub version: String,
    /// The folder a new installation goes to.
    pub dir: PathBuf,
    /// What is installed already; `None` for a new installation.
    pub installed: Option<Installed>,
    /// This Setup carries a payload (an upgrade can be offered).
    pub can_upgrade: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WelcomeChoice {
    Install,
    ChangeFolder,
    Upgrade,
    ChangeAccount,
    Remove,
    Cancel,
}

/// Which radio button is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSel {
    BuiltIn,
    Windows,
    /// A passwordless account other than the built-in one, given with `/account`.
    Given,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountPage {
    /// The account the service runs as now (an account change).
    pub current: Option<String>,
    /// `/account`'s passwordless account, offered as a third choice.
    pub given: Option<String>,
    pub selected: AccountSel,
    /// Why the last account was not accepted.
    pub error: Option<String>,
    /// There is a page to go back to.
    pub back: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountAnswer {
    BuiltIn,
    /// Open the credential prompt.
    Windows,
    Given,
    Back,
    Cancel,
}

/// The Windows credential prompt, pre-filled with `user`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredRequest {
    pub user: Option<String>,
}

#[derive(Debug)]
pub struct Credentials {
    pub user: String,
    pub password: Secret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    Install,
    Upgrade,
    ChangeAccount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPage {
    pub kind: SummaryKind,
    pub version: String,
    pub dir: PathBuf,
    /// The account the service will run as.
    pub account: String,
    /// The account is the one the service has (an upgrade).
    pub account_kept: bool,
    /// The panel's port the firewall rule opens.
    pub firewall: Option<u16>,
    /// The installer's own warnings, as it says them.
    pub notes: Vec<String>,
    /// The share question's text; when present the default button is Back.
    pub share: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Confirm,
    Back,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    Install,
    Upgrade,
    ChangeAccount,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressPage {
    pub work: Work,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovePage {
    /// What is removed and kept ([`crate::run::uninstall_summary`]).
    pub lines: Vec<String>,
    pub remove_data: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveAnswer {
    Remove { remove_data: bool },
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishedPage {
    /// A new installation; the admin page's address. `kept_database`: the
    /// folder held a database, so there is no first-run Admin / Admin (Y6).
    Installed { admin_url: Option<String>, kept_database: bool },
    Upgraded { version: String },
    AccountChanged { account: String },
    Removed { sentence: String },
    RolledBack { message: String, running: bool },
    Failed { message: String, log: Option<PathBuf> },
    Refused { notice: Vec<String>, message: String },
}

/// What the progress page shows while the work runs on another thread: the
/// installer's lines as they arrive and the step of the last one.
#[derive(Debug, Default)]
pub struct Feed {
    state: Mutex<FeedState>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FeedState {
    pub step: Option<Step>,
    pub lines: Vec<String>,
}

impl Feed {
    pub fn line(&self, step: Step, text: &str) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.step = Some(step);
        s.lines.push(text.to_string());
    }

    pub fn snapshot(&self) -> FeedState {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// The work a progress page runs on a worker thread.
pub type Job<'a> = Box<dyn FnOnce(&Feed) -> Outcome + Send + 'a>;

/// Setup's pages. Cancel on any page before the summary is confirmed changes nothing.
pub trait Ui {
    fn welcome(&mut self, page: &WelcomePage) -> WelcomeChoice;
    /// The system folder picker; `None` when cancelled.
    fn pick_folder(&mut self, current: &Path) -> Option<PathBuf>;
    fn account(&mut self, page: &AccountPage) -> AccountAnswer;
    /// The Windows credential prompt; `None` when cancelled.
    fn credentials(&mut self, request: &CredRequest) -> Option<Credentials>;
    fn summary(&mut self, page: &SummaryPage) -> Nav;
    /// Run `job` while the page shows its progress; the page cannot be closed
    /// until it has returned.
    fn progress(&mut self, page: &ProgressPage, job: Job<'_>) -> Outcome;
    fn remove(&mut self, page: &RemovePage) -> RemoveAnswer;
    fn finished(&mut self, page: &FinishedPage);
}

// ---- rendering -----------------------------------------------------------------

/// `IDCANCEL`: the Cancel button, Esc and the close box.
pub const ID_CANCEL: i32 = 2;
pub const ID_INSTALL: i32 = 101;
pub const ID_FOLDER: i32 = 102;
pub const ID_UPGRADE: i32 = 103;
pub const ID_CHANGE_ACCOUNT: i32 = 104;
pub const ID_REMOVE: i32 = 105;
pub const ID_NEXT: i32 = 110;
pub const ID_BACK: i32 = 111;
pub const ID_CONFIRM: i32 = 112;
pub const ID_CLOSE: i32 = 113;
/// The progress page's only button, disabled until the work is done.
pub const ID_WAIT: i32 = 114;
pub const RADIO_BUILTIN: i32 = 201;
pub const RADIO_WINDOWS: i32 = 202;
pub const RADIO_GIVEN: i32 = 203;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Info,
    Warning,
    Error,
    Shield,
}

/// One task dialog, every string in the page's language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    pub title: String,
    pub icon: Icon,
    pub main: String,
    pub content: String,
    /// Custom buttons, in order; command links when `command_links`.
    pub buttons: Vec<(i32, String)>,
    pub command_links: bool,
    pub default_button: i32,
    /// Esc and the close box mean [`ID_CANCEL`].
    pub cancellable: bool,
    pub radios: Vec<(i32, String)>,
    pub default_radio: i32,
    /// The checkbox under the buttons, and whether it starts checked.
    pub verification: Option<(String, bool)>,
    /// The expandable details area: (text, "show" label, "hide" label).
    pub expanded: Option<(String, String, String)>,
}

impl Dialog {
    fn new(lang: Lang, version: &str, icon: Icon, main: String, content: String) -> Self {
        Dialog {
            title: fmt(lang, T::Title, &[version]),
            icon,
            main,
            content,
            buttons: Vec::new(),
            command_links: false,
            default_button: ID_CANCEL,
            cancellable: true,
            radios: Vec::new(),
            default_radio: 0,
            verification: None,
            expanded: None,
        }
    }

    fn button(mut self, id: i32, text: impl Into<String>) -> Self {
        self.buttons.push((id, text.into()));
        self
    }

    fn cancel(self, lang: Lang) -> Self {
        self.button(ID_CANCEL, tr(lang, T::BtnCancel))
    }
}

fn path(p: &Path) -> String {
    p.display().to_string()
}

pub fn render_welcome(page: &WelcomePage, lang: Lang) -> Dialog {
    let v = page.version.as_str();
    match &page.installed {
        None => {
            let mut d = Dialog::new(
                lang,
                v,
                Icon::Info,
                fmt(lang, T::WelcomeNewMain, &[v]),
                fmt(lang, T::WelcomeNewContent, &[&path(&page.dir)]),
            )
            .button(ID_INSTALL, tr(lang, T::LinkInstall))
            .button(ID_FOLDER, tr(lang, T::LinkChangeFolder))
            .cancel(lang);
            d.command_links = true;
            d.default_button = ID_INSTALL;
            d
        }
        Some(found) => {
            let mut lines = vec![fmt(lang, T::FoundFolder, &[&path(&found.dir)])];
            if let Some(version) = &found.version {
                lines.push(fmt(lang, T::FoundVersion, &[version]));
            }
            match (&found.account, found.service) {
                (Some(a), true) => lines.push(fmt(lang, T::FoundAccount, &[a])),
                (_, false) => lines.push(tr(lang, T::FoundNoService).into()),
                (None, true) => {}
            }
            lines.push(String::new());
            lines.push(tr(lang, T::FoundWhat).into());
            let mut d = Dialog::new(lang, v, Icon::Info, tr(lang, T::WelcomeFoundMain).into(), lines.join("\n"));
            if page.can_upgrade {
                d = d.button(ID_UPGRADE, fmt(lang, T::LinkUpgrade, &[v]));
                d.default_button = ID_UPGRADE;
            }
            let mut d = d.button(ID_CHANGE_ACCOUNT, tr(lang, T::LinkChangeAccount)).button(ID_REMOVE, tr(lang, T::LinkRemove)).cancel(lang);
            d.command_links = true;
            d
        }
    }
}

pub fn welcome_choice(button: i32) -> WelcomeChoice {
    match button {
        ID_INSTALL => WelcomeChoice::Install,
        ID_FOLDER => WelcomeChoice::ChangeFolder,
        ID_UPGRADE => WelcomeChoice::Upgrade,
        ID_CHANGE_ACCOUNT => WelcomeChoice::ChangeAccount,
        ID_REMOVE => WelcomeChoice::Remove,
        _ => WelcomeChoice::Cancel,
    }
}

pub fn render_account(page: &AccountPage, lang: Lang, version: &str) -> Dialog {
    let mut content = Vec::new();
    if let Some(c) = &page.current {
        content.push(fmt(lang, T::AccountNow, &[c]));
        content.push(String::new());
    }
    content.push(tr(lang, T::AccountContent).into());
    if let Some(e) = &page.error {
        content.push(String::new());
        content.push(tr(lang, T::AccountRefused).into());
        content.push(e.clone());
    }
    let icon = if page.error.is_some() { Icon::Warning } else { Icon::Info };
    let mut d = Dialog::new(lang, version, icon, tr(lang, T::AccountMain).into(), content.join("\n"));
    d.radios.push((RADIO_BUILTIN, tr(lang, T::RadioBuiltIn).into()));
    d.radios.push((RADIO_WINDOWS, tr(lang, T::RadioWindows).into()));
    if let Some(g) = &page.given {
        d.radios.push((RADIO_GIVEN, fmt(lang, T::RadioGiven, &[g])));
    }
    d.default_radio = match page.selected {
        AccountSel::BuiltIn => RADIO_BUILTIN,
        AccountSel::Windows => RADIO_WINDOWS,
        AccountSel::Given if page.given.is_some() => RADIO_GIVEN,
        AccountSel::Given => RADIO_BUILTIN,
    };
    if page.back {
        d = d.button(ID_BACK, tr(lang, T::BtnBack));
    }
    let mut d = d.button(ID_NEXT, tr(lang, T::BtnNext)).cancel(lang);
    d.default_button = ID_NEXT;
    d
}

pub fn account_answer(button: i32, radio: i32) -> AccountAnswer {
    match (button, radio) {
        (ID_BACK, _) => AccountAnswer::Back,
        (ID_NEXT, RADIO_WINDOWS) => AccountAnswer::Windows,
        (ID_NEXT, RADIO_GIVEN) => AccountAnswer::Given,
        (ID_NEXT, _) => AccountAnswer::BuiltIn,
        _ => AccountAnswer::Cancel,
    }
}

pub fn render_summary(page: &SummaryPage, lang: Lang) -> Dialog {
    let v = page.version.as_str();
    let dir = path(&page.dir);
    let account = page.account.as_str();
    let firewall = |lines: &mut Vec<String>| match page.firewall {
        Some(port) => lines.push(fmt(lang, T::SumFirewall, &[&port.to_string()])),
        None => lines.push(tr(lang, T::SumNoFirewall).into()),
    };
    let mut lines = Vec::new();
    // The share question comes first; its answer is No unless the operator
    // presses the affirmative button (the console's default, P13.1c round 2).
    let asks_share = !page.share.is_empty();
    if asks_share {
        lines.push(tr(lang, T::ShareHeading).into());
        lines.extend(page.share.iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()));
        lines.push(tr(lang, T::ShareAdvice).into());
        lines.push(String::new());
    }
    let (main, confirm) = match page.kind {
        SummaryKind::Install => {
            lines.push(fmt(lang, T::SumFolder, &[&dir]));
            lines.push(fmt(lang, T::SumAccount, &[account]));
            lines.push(tr(lang, T::SumService).into());
            firewall(&mut lines);
            (fmt(lang, T::SummaryInstallMain, &[v]), T::BtnInstall)
        }
        SummaryKind::Upgrade => {
            lines.push(fmt(lang, T::SumUpgradeFolder, &[&dir]));
            if page.account_kept {
                lines.push(tr(lang, T::SumKeeps).into());
            } else {
                lines.push(tr(lang, T::SumKeepsData).into());
                lines.push(fmt(lang, T::SumAccount, &[account]));
            }
            firewall(&mut lines);
            lines.push(tr(lang, T::SumUpgradeBack).into());
            (fmt(lang, T::SummaryUpgradeMain, &[v]), T::BtnUpgrade)
        }
        SummaryKind::ChangeAccount => {
            lines.push(fmt(lang, T::SumChangeFolder, &[&dir]));
            lines.push(fmt(lang, T::SumAccount, &[account]));
            lines.push(tr(lang, T::SumChangeBack).into());
            (tr(lang, T::SummaryChangeMain).into(), T::BtnChange)
        }
    };
    if !page.notes.is_empty() {
        lines.push(String::new());
        lines.push(tr(lang, T::SumNotes).into());
        lines.extend(page.notes.iter().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()));
    }
    let icon = if page.notes.is_empty() && !asks_share { Icon::Shield } else { Icon::Warning };
    let mut d = Dialog::new(lang, v, icon, main, lines.join("\n"))
        .button(ID_BACK, tr(lang, T::BtnBack))
        .button(ID_CONFIRM, tr(lang, confirm))
        .cancel(lang);
    // Enter and `/auto` never answer the share question yes.
    d.default_button = if asks_share { ID_BACK } else { ID_CONFIRM };
    d
}

pub fn nav(button: i32) -> Nav {
    match button {
        ID_CONFIRM => Nav::Confirm,
        ID_BACK => Nav::Back,
        _ => Nav::Cancel,
    }
}

pub fn render_progress(page: &ProgressPage, lang: Lang) -> Dialog {
    let v = page.version.as_str();
    let main = match page.work {
        Work::Install => fmt(lang, T::ProgInstallMain, &[v]),
        Work::Upgrade => fmt(lang, T::ProgUpgradeMain, &[v]),
        Work::ChangeAccount => tr(lang, T::ProgChangeMain).into(),
        Work::Remove => tr(lang, T::ProgRemoveMain).into(),
    };
    let mut d = Dialog::new(lang, v, Icon::Info, main, progress_content(page, lang, &FeedState::default()))
        .button(ID_WAIT, tr(lang, T::BtnWait));
    d.default_button = ID_WAIT;
    d.cancellable = false;
    d.expanded = Some((" ".into(), tr(lang, T::DetailsShow).into(), tr(lang, T::DetailsHide).into()));
    d
}

/// The progress page's text for what the feed holds now.
pub fn progress_content(page: &ProgressPage, lang: Lang, state: &FeedState) -> String {
    let mut out = String::new();
    if page.work != Work::Remove {
        out.push_str(tr(lang, T::ProgWait));
        if let Some(step) = state.step {
            out.push_str("\n\n");
            out.push_str(&fmt(lang, T::ProgNow, &[tr(lang, step_key(step))]));
        }
    }
    out
}

/// The details area: the installer's last lines, as it said them.
pub fn progress_details(state: &FeedState) -> String {
    const SHOWN: usize = 25;
    let lines: Vec<&str> = state.lines.iter().map(|l| l.trim_end()).filter(|l| !l.trim().is_empty()).collect();
    let text = lines[lines.len().saturating_sub(SHOWN)..].join("\n");
    if text.is_empty() {
        " ".into()
    } else {
        text
    }
}

pub fn render_remove(page: &RemovePage, lang: Lang, version: &str) -> Dialog {
    let lines: Vec<&str> = page.lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    let mut d = Dialog::new(lang, version, Icon::Warning, tr(lang, T::RemoveMain).into(), lines.join("\n"))
        .button(ID_REMOVE, tr(lang, T::BtnRemove))
        .cancel(lang);
    // Enter and `/auto` never delete the station's data (Y1).
    d.default_button = if page.remove_data { ID_CANCEL } else { ID_REMOVE };
    d.verification = Some((tr(lang, T::RemoveData).into(), page.remove_data));
    d
}

pub fn remove_answer(button: i32, checked: bool) -> RemoveAnswer {
    match button {
        ID_REMOVE => RemoveAnswer::Remove { remove_data: checked },
        _ => RemoveAnswer::Cancel,
    }
}

pub fn render_finished(page: &FinishedPage, lang: Lang, version: &str) -> Dialog {
    // The installer's own sentences start with a mark for the console.
    let plain = |s: &str| s.trim().trim_start_matches(['✓', '✗', '!']).trim().to_string();
    let (icon, main, content) = match page {
        FinishedPage::Installed { admin_url, kept_database } => {
            let key = if *kept_database { T::FinInstalledKeptContent } else { T::FinInstalledContent };
            let mut c = tr(lang, key).to_string();
            if let Some(url) = admin_url {
                c.push_str("\n\n");
                c.push_str(&fmt(lang, T::FinAddress, &[url]));
            }
            (Icon::Info, tr(lang, T::FinInstalledMain).to_string(), c)
        }
        FinishedPage::Upgraded { version } => (Icon::Info, fmt(lang, T::FinUpgraded, &[version]), String::new()),
        FinishedPage::AccountChanged { account } => (Icon::Info, fmt(lang, T::FinAccount, &[account]), String::new()),
        FinishedPage::Removed { sentence } => (Icon::Info, tr(lang, T::FinRemovedMain).into(), plain(sentence)),
        FinishedPage::RolledBack { message, running: true } => (Icon::Warning, tr(lang, T::FinRolledBackMain).into(), message.clone()),
        FinishedPage::RolledBack { message, running: false } => (Icon::Error, tr(lang, T::FinNotBackMain).into(), message.clone()),
        FinishedPage::Failed { message, log } => {
            let mut c = message.clone();
            if let Some(l) = log {
                c.push_str("\n\n");
                c.push_str(&fmt(lang, T::FinLog, &[&path(l)]));
            }
            (Icon::Error, tr(lang, T::FinFailedMain).into(), c)
        }
        FinishedPage::Refused { notice, message } => {
            let mut lines: Vec<String> = notice.iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            lines.push(message.clone());
            lines.push(String::new());
            lines.push(tr(lang, T::FinNothingChanged).into());
            (Icon::Warning, tr(lang, T::FinRefusedMain).into(), lines.join("\n"))
        }
    };
    let mut d = Dialog::new(lang, version, icon, main, content).button(ID_CLOSE, tr(lang, T::BtnClose));
    d.default_button = ID_CLOSE;
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_never_printed() {
        let s = Secret::new("Tr0ub4dor-test-only".into());
        assert_eq!(format!("{s:?}"), "<hidden>");
        let c = Credentials { user: r"NEWSROOM\svc_test".into(), password: s };
        assert!(!format!("{c:?}").contains("Tr0ub4dor"));
        let mut buf = *b"Tr0ub4dor";
        zero_bytes(&mut buf);
        assert_eq!(buf, [0u8; 9]);
    }

    #[test]
    fn every_page_has_the_product_and_version_in_its_title_and_a_safe_way_out() {
        let v = "1.0.0";
        let welcome = WelcomePage { version: v.into(), dir: PathBuf::from(r"C:\OmniIngest"), installed: None, can_upgrade: true };
        let summary = SummaryPage {
            kind: SummaryKind::Install,
            version: v.into(),
            dir: PathBuf::from(r"C:\OmniIngest"),
            account: "NT SERVICE\\OmniIngestService".into(),
            account_kept: false,
            firewall: Some(8080),
            notes: vec![],
            share: vec![],
        };
        let account = AccountPage { current: None, given: None, selected: AccountSel::BuiltIn, error: None, back: true };
        let remove = RemovePage { lines: vec!["  Removes: the program files.".into()], remove_data: false };
        for lang in [Lang::En, Lang::El] {
            let pages = [
                render_welcome(&welcome, lang),
                render_account(&account, lang, v),
                render_summary(&summary, lang),
                render_remove(&remove, lang, v),
            ];
            for d in &pages {
                assert_eq!(d.title, fmt(lang, T::Title, &[v]));
                assert!(d.cancellable && d.buttons.iter().any(|(id, _)| *id == ID_CANCEL), "{d:?}");
            }
            let p = render_progress(&ProgressPage { work: Work::Install, version: v.into() }, lang);
            assert!(!p.cancellable && p.buttons == [(ID_WAIT, tr(lang, T::BtnWait).to_string())], "the work cannot be closed");
        }
        assert_eq!(render_remove(&remove, Lang::En, v).verification, Some((T::RemoveData.en().into(), false)));
    }

    #[test]
    fn the_choices_come_back_from_the_buttons() {
        assert_eq!(welcome_choice(ID_FOLDER), WelcomeChoice::ChangeFolder);
        assert_eq!(welcome_choice(ID_CANCEL), WelcomeChoice::Cancel);
        assert_eq!(account_answer(ID_NEXT, RADIO_WINDOWS), AccountAnswer::Windows);
        assert_eq!(account_answer(ID_NEXT, RADIO_BUILTIN), AccountAnswer::BuiltIn);
        assert_eq!(account_answer(ID_CANCEL, RADIO_WINDOWS), AccountAnswer::Cancel);
        assert_eq!(nav(ID_CONFIRM), Nav::Confirm);
        assert_eq!(nav(ID_CANCEL), Nav::Cancel);
        assert_eq!(remove_answer(ID_REMOVE, true), RemoveAnswer::Remove { remove_data: true });
        assert_eq!(remove_answer(ID_CANCEL, true), RemoveAnswer::Cancel);
    }

    #[test]
    fn the_finished_page_shows_the_installer_s_own_sentences() {
        let d = render_finished(&FinishedPage::RolledBack { message: "The upgrade failed (x) and the previous version was put back; it is running.".into(), running: true }, Lang::El, "1.0.0");
        assert_eq!(d.icon, Icon::Warning);
        assert_eq!(d.main, T::FinRolledBackMain.el());
        assert!(d.content.contains("previous version was put back"));
        let d = render_finished(&FinishedPage::Failed { message: "boom".into(), log: Some(PathBuf::from(r"C:\OmniIngest\setup.log")) }, Lang::En, "1.0.0");
        assert_eq!(d.icon, Icon::Error);
        assert!(d.content.contains(r"The details are in C:\OmniIngest\setup.log."), "{}", d.content);
        let d = render_finished(&FinishedPage::Removed { sentence: "✓ OmniIngest was removed.".into() }, Lang::En, "1.0.0");
        assert_eq!(d.content, "OmniIngest was removed.");
        let d = render_finished(&FinishedPage::Installed { admin_url: Some("http://127.0.0.1:8080/admin".into()), kept_database: false }, Lang::En, "1.0.0");
        assert!(d.content.contains("Admin / Admin") && d.content.ends_with("Admin page: http://127.0.0.1:8080/admin"));
    }

    #[test]
    fn the_progress_text_follows_the_feed() {
        let feed = Feed::default();
        feed.line(Step::Copy, "  Copied omni-ingest.exe");
        feed.line(Step::Start, "  Starting the service…");
        let s = feed.snapshot();
        let page = ProgressPage { work: Work::Install, version: "1.0.0".into() };
        assert!(progress_content(&page, Lang::En, &s).ends_with("Now: Starting the service"));
        assert_eq!(progress_details(&s), "  Copied omni-ingest.exe\n  Starting the service…");
    }
}
