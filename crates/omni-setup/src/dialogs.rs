//! The binary's Win32 pages (P13.1c): [`TaskDialogUi`] shows each page
//! rendered by `omni_deploy::ui` as a task dialog (common controls 6), opens
//! the Windows credential prompt and the system folder picker, and shows a
//! message box where there is no console. Thin and not unit-tested: what a
//! page says and what its buttons mean are decided, and tested, in the library.
//! It lives in the binary so the library's tests never link the task dialog.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use omni_deploy::backend::Outcome;
use omni_deploy::exit::ExitCode;
use omni_deploy::texts::{fmt, tr, Lang, T};
use omni_deploy::ui::{
    account_answer, nav, remove_answer, render_account, render_finished, render_progress, render_remove, render_summary,
    render_welcome, welcome_choice, AccountAnswer, AccountPage, CredRequest, Credentials, Feed, FinishedPage, Job, Nav,
    ProgressPage, RemoveAnswer, RemovePage, SummaryPage, Ui, WelcomeChoice, WelcomePage,
};

/// The pages as task dialogs. `auto`: every page answers itself with its
/// default button after about 300 ms, and the credential prompt and folder
/// picker are skipped (`/pages-demo … /auto`).
pub struct TaskDialogUi {
    pub lang: Lang,
    pub auto: bool,
    pub version: String,
    /// `/auto` met a page it cannot answer by itself (the skipped credential
    /// prompt): every later page answers Cancel, so the walk ends.
    pub abandoned: bool,
}

impl TaskDialogUi {
    pub fn new(lang: Lang, auto: bool) -> Self {
        TaskDialogUi { lang, auto, version: omni_deploy::run::VERSION.into(), abandoned: false }
    }

    /// `d` as `/auto` answers it.
    fn auto(&self, mut d: omni_deploy::ui::Dialog) -> omni_deploy::ui::Dialog {
        if self.auto && self.abandoned {
            d.default_button = omni_deploy::ui::ID_CANCEL;
        }
        d
    }
}

/// What a dialog was closed with.
#[derive(Debug, Clone, Copy)]
struct Closed {
    button: i32,
    radio: i32,
    checked: bool,
}

/// Sets the flag when dropped, so a panicking job still lets the page close.
struct DoneOnDrop<'a>(&'a AtomicBool);

impl Drop for DoneOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The progress page's view of the running job.
struct Watch<'a> {
    feed: &'a Feed,
    done: &'a AtomicBool,
    page: &'a ProgressPage,
}

impl Ui for TaskDialogUi {
    fn welcome(&mut self, page: &WelcomePage) -> WelcomeChoice {
        welcome_choice(sys::show(&self.auto(render_welcome(page, self.lang)), self.auto, None, self.lang).button)
    }

    fn pick_folder(&mut self, current: &Path) -> Option<PathBuf> {
        if self.auto {
            return None;
        }
        sys::pick_folder(current, tr(self.lang, T::FolderTitle))
    }

    fn account(&mut self, page: &AccountPage) -> AccountAnswer {
        let c = sys::show(&self.auto(render_account(page, self.lang, &self.version)), self.auto, None, self.lang);
        account_answer(c.button, c.radio)
    }

    fn credentials(&mut self, request: &CredRequest) -> Option<Credentials> {
        if self.auto {
            self.abandoned = true;
            return None;
        }
        sys::credentials(request, tr(self.lang, T::CredCaption), tr(self.lang, T::CredMessage))
    }

    fn summary(&mut self, page: &SummaryPage) -> Nav {
        nav(sys::show(&self.auto(render_summary(page, self.lang)), self.auto, None, self.lang).button)
    }

    fn progress(&mut self, page: &ProgressPage, job: Job<'_>) -> Outcome {
        let feed = Feed::default();
        let done = AtomicBool::new(false);
        let dialog = render_progress(page, self.lang);
        std::thread::scope(|s| {
            let worker = s.spawn(|| {
                let _done = DoneOnDrop(&done);
                job(&feed)
            });
            // Shown until the job is done; it cannot be closed before.
            sys::show(&dialog, false, Some(Watch { feed: &feed, done: &done, page }), self.lang);
            worker.join().unwrap_or_else(|_| Outcome::Failed {
                code: ExitCode::Failed,
                message: tr(self.lang, T::WorkerStopped).into(),
                log: None,
            })
        })
    }

    fn remove(&mut self, page: &RemovePage) -> RemoveAnswer {
        let c = sys::show(&self.auto(render_remove(page, self.lang, &self.version)), self.auto, None, self.lang);
        remove_answer(c.button, c.checked)
    }

    fn finished(&mut self, page: &FinishedPage) {
        sys::show(&render_finished(page, self.lang, &self.version), self.auto, None, self.lang);
    }
}

/// Per-monitor DPI awareness and COM, before the first page.
pub fn prepare() {
    sys::prepare()
}

/// One message, without a console.
pub fn message_box(lang: Lang, text: &str, error: bool) {
    sys::message_box(&fmt(lang, T::Title, &[omni_deploy::run::VERSION]), text, error)
}

#[cfg(windows)]
mod sys {
    use super::{Closed, Watch};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    use omni_deploy::texts::Lang;
    use omni_deploy::ui::{
        progress_content, progress_details, zero_bytes, zero_u16, CredRequest, Credentials, Dialog, Icon, Secret, ID_CANCEL,
        ID_WAIT,
    };
    use windows::core::{HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, S_FALSE, S_OK, WPARAM};
    use windows::Win32::Graphics::Gdi::HBITMAP;
    use windows::Win32::Security::Credentials::{
        CredPackAuthenticationBufferW, CredUIPromptForWindowsCredentialsW, CredUnPackAuthenticationBufferW, CREDUIWIN_GENERIC,
        CREDUI_INFOW, CRED_PACK_FLAGS, CRED_PACK_GENERIC_CREDENTIALS, CRED_PACK_PROTECTED_CREDENTIALS,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, IBindCtx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
        COINIT_DISABLE_OLE1DDE,
    };
    use windows::Win32::UI::Controls::{
        TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOG_BUTTON, TASKDIALOG_FLAGS, TASKDIALOG_NOTIFICATIONS,
        TDE_CONTENT, TDE_EXPANDED_INFORMATION, TDF_ALLOW_DIALOG_CANCELLATION, TDF_CALLBACK_TIMER, TDF_SHOW_MARQUEE_PROGRESS_BAR,
        TDF_USE_COMMAND_LINKS, TDF_VERIFICATION_FLAG_CHECKED, TDM_CLICK_BUTTON, TDM_ENABLE_BUTTON, TDM_SET_ELEMENT_TEXT,
        TDM_SET_PROGRESS_BAR_MARQUEE, TDN_BUTTON_CLICKED, TDN_CREATED, TDN_TIMER, TD_ERROR_ICON, TD_INFORMATION_ICON,
        TD_SHIELD_ICON, TD_WARNING_ICON,
    };
    use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
        SIGDN_FILESYSPATH,
    };
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, SendMessageW, MB_ICONERROR, MB_ICONWARNING, MB_OK};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn wide_path(p: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    fn until_nul(units: &[u16]) -> &[u16] {
        &units[..units.iter().position(|&c| c == 0).unwrap_or(units.len())]
    }

    pub fn prepare() {
        // SAFETY: process-wide settings, made on the main thread before any window.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        }
    }

    pub fn message_box(title: &str, text: &str, error: bool) {
        let (t, x) = (wide(title), wide(text));
        let icon = if error { MB_ICONERROR } else { MB_ICONWARNING };
        // SAFETY: both strings outlive the call.
        unsafe {
            MessageBoxW(HWND::default(), PCWSTR(x.as_ptr()), PCWSTR(t.as_ptr()), MB_OK | icon);
        }
    }

    /// What the callback works with while one dialog is open.
    struct Cb<'a> {
        auto: bool,
        default_button: i32,
        clicked: bool,
        watch: Option<Watch<'a>>,
        lang: Lang,
        /// The texts last set; kept alive while the dialog may read them.
        content: Vec<u16>,
        details: Vec<u16>,
        shown: (String, String),
    }

    fn send(hwnd: HWND, msg: i32, wparam: usize, lparam: isize) {
        // SAFETY: a task dialog message to the dialog that called us.
        unsafe {
            SendMessageW(hwnd, msg as u32, WPARAM(wparam), LPARAM(lparam));
        }
    }

    unsafe extern "system" fn callback(
        hwnd: HWND,
        msg: TASKDIALOG_NOTIFICATIONS,
        wparam: WPARAM,
        _lparam: LPARAM,
        data: isize,
    ) -> HRESULT {
        // SAFETY: `data` is the `Cb` of `show`, which outlives the dialog.
        let cb = unsafe { &mut *(data as *mut Cb) };
        match msg {
            TDN_CREATED => {
                if cb.watch.is_some() {
                    send(hwnd, TDM_SET_PROGRESS_BAR_MARQUEE.0, 1, 30);
                    send(hwnd, TDM_ENABLE_BUTTON.0, ID_WAIT as usize, 0);
                }
            }
            TDN_TIMER => {
                if let Some(w) = &cb.watch {
                    let state = w.feed.snapshot();
                    let content = progress_content(w.page, cb.lang, &state);
                    let details = progress_details(&state);
                    if content != cb.shown.0 {
                        cb.content = wide(&content);
                        send(hwnd, TDM_SET_ELEMENT_TEXT.0, TDE_CONTENT.0 as usize, cb.content.as_ptr() as isize);
                        cb.shown.0 = content;
                    }
                    if details != cb.shown.1 {
                        cb.details = wide(&details);
                        send(hwnd, TDM_SET_ELEMENT_TEXT.0, TDE_EXPANDED_INFORMATION.0 as usize, cb.details.as_ptr() as isize);
                        cb.shown.1 = details;
                    }
                    if w.done.load(Ordering::SeqCst) && !cb.clicked {
                        cb.clicked = true;
                        send(hwnd, TDM_ENABLE_BUTTON.0, ID_WAIT as usize, 1);
                        send(hwnd, TDM_CLICK_BUTTON.0, ID_WAIT as usize, 0);
                    }
                } else if cb.auto && !cb.clicked && wparam.0 >= 300 {
                    cb.clicked = true;
                    send(hwnd, TDM_CLICK_BUTTON.0, cb.default_button as usize, 0);
                }
            }
            TDN_BUTTON_CLICKED => {
                // A half-finished install is worse than a wait: nothing closes
                // the progress page before the work is done.
                if cb.watch.as_ref().is_some_and(|w| !w.done.load(Ordering::SeqCst)) {
                    return S_FALSE;
                }
            }
            _ => {}
        }
        S_OK
    }

    pub fn show(d: &Dialog, auto: bool, watch: Option<Watch<'_>>, lang: Lang) -> Closed {
        let title = wide(&d.title);
        let main = wide(&d.main);
        let content = wide(&d.content);
        let button_text: Vec<Vec<u16>> = d.buttons.iter().map(|(_, t)| wide(t)).collect();
        let buttons: Vec<TASKDIALOG_BUTTON> = d
            .buttons
            .iter()
            .zip(&button_text)
            .map(|((id, _), t)| TASKDIALOG_BUTTON { nButtonID: *id, pszButtonText: PCWSTR(t.as_ptr()) })
            .collect();
        let radio_text: Vec<Vec<u16>> = d.radios.iter().map(|(_, t)| wide(t)).collect();
        let radios: Vec<TASKDIALOG_BUTTON> = d
            .radios
            .iter()
            .zip(&radio_text)
            .map(|((id, _), t)| TASKDIALOG_BUTTON { nButtonID: *id, pszButtonText: PCWSTR(t.as_ptr()) })
            .collect();
        let verification = d.verification.as_ref().map(|(t, _)| wide(t));
        let expanded = d.expanded.as_ref().map(|(t, show, hide)| (wide(t), wide(show), wide(hide)));
        let progress = watch.is_some();
        let mut flags = TDF_CALLBACK_TIMER;
        if d.command_links {
            flags |= TDF_USE_COMMAND_LINKS;
        }
        if d.cancellable {
            flags |= TDF_ALLOW_DIALOG_CANCELLATION;
        }
        if progress {
            flags |= TDF_SHOW_MARQUEE_PROGRESS_BAR;
        }
        if d.verification.as_ref().is_some_and(|(_, checked)| *checked) {
            flags |= TDF_VERIFICATION_FLAG_CHECKED;
        }
        let icon = match d.icon {
            Icon::Info => TD_INFORMATION_ICON,
            Icon::Warning => TD_WARNING_ICON,
            Icon::Error => TD_ERROR_ICON,
            Icon::Shield => TD_SHIELD_ICON,
        };
        let mut cb = Cb {
            auto,
            default_button: d.default_button,
            clicked: false,
            watch,
            lang,
            content: Vec::new(),
            details: Vec::new(),
            shown: (d.content.clone(), String::new()),
        };
        let none = PCWSTR::null();
        let config = TASKDIALOGCONFIG {
            cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
            dwFlags: TASKDIALOG_FLAGS(flags.0),
            pszWindowTitle: PCWSTR(title.as_ptr()),
            Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: icon },
            pszMainInstruction: PCWSTR(main.as_ptr()),
            pszContent: if d.content.is_empty() { none } else { PCWSTR(content.as_ptr()) },
            cButtons: buttons.len() as u32,
            pButtons: buttons.as_ptr(),
            nDefaultButton: d.default_button,
            cRadioButtons: radios.len() as u32,
            pRadioButtons: if radios.is_empty() { std::ptr::null() } else { radios.as_ptr() },
            nDefaultRadioButton: d.default_radio,
            pszVerificationText: verification.as_ref().map_or(none, |v| PCWSTR(v.as_ptr())),
            pszExpandedInformation: expanded.as_ref().map_or(none, |e| PCWSTR(e.0.as_ptr())),
            pszCollapsedControlText: expanded.as_ref().map_or(none, |e| PCWSTR(e.1.as_ptr())),
            pszExpandedControlText: expanded.as_ref().map_or(none, |e| PCWSTR(e.2.as_ptr())),
            pfCallback: Some(callback),
            lpCallbackData: &mut cb as *mut Cb as isize,
            ..Default::default()
        };
        let (mut button, mut radio) = (ID_CANCEL, d.default_radio);
        let mut checked = windows::Win32::Foundation::BOOL(0);
        // SAFETY: every string, the button arrays and `cb` outlive the modal call.
        let shown = unsafe { TaskDialogIndirect(&config, Some(&mut button), Some(&mut radio), Some(&mut checked)) };
        if shown.is_err() {
            button = ID_CANCEL;
        }
        Closed { button, radio, checked: checked.as_bool() }
    }

    pub fn credentials(request: &CredRequest, caption: &str, message: &str) -> Option<Credentials> {
        let (caption, message) = (wide(caption), wide(message));
        let info = CREDUI_INFOW {
            cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
            hwndParent: HWND::default(),
            pszMessageText: PCWSTR(message.as_ptr()),
            pszCaptionText: PCWSTR(caption.as_ptr()),
            hbmBanner: HBITMAP::default(),
        };
        // The user name to start with, packed with an empty password.
        let mut prefill: Vec<u8> = Vec::new();
        if let Some(user) = &request.user {
            let (u, empty) = (wide(user), wide(""));
            let mut size = 0u32;
            // SAFETY: the first call only reports the size the second fills.
            unsafe {
                let _ = CredPackAuthenticationBufferW(CRED_PACK_GENERIC_CREDENTIALS, PCWSTR(u.as_ptr()), PCWSTR(empty.as_ptr()), None, &mut size);
                if size > 0 {
                    prefill = vec![0u8; size as usize];
                    let packed = CredPackAuthenticationBufferW(
                        CRED_PACK_GENERIC_CREDENTIALS,
                        PCWSTR(u.as_ptr()),
                        PCWSTR(empty.as_ptr()),
                        Some(prefill.as_mut_ptr()),
                        &mut size,
                    );
                    if packed.is_err() {
                        prefill.clear();
                    }
                }
            }
        }
        let mut package = 0u32;
        let mut out: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut out_size = 0u32;
        // SAFETY: `info` and `prefill` outlive the call; the out buffer is ours to free.
        let rc = unsafe {
            CredUIPromptForWindowsCredentialsW(
                Some(&info),
                0,
                &mut package,
                (!prefill.is_empty()).then(|| prefill.as_ptr() as *const std::ffi::c_void),
                prefill.len() as u32,
                &mut out,
                &mut out_size,
                None,
                CREDUIWIN_GENERIC,
            )
        };
        zero_bytes(&mut prefill);
        if rc != 0 || out.is_null() {
            return None;
        }
        let mut user = vec![0u16; 514];
        let mut domain = vec![0u16; 338];
        let mut password = vec![0u16; 257];
        let mut unpacked = false;
        for flags in [CRED_PACK_PROTECTED_CREDENTIALS, CRED_PACK_FLAGS(0)] {
            let (mut ul, mut dl, mut pl) = (user.len() as u32, domain.len() as u32, password.len() as u32);
            // SAFETY: each buffer is passed with its length in characters.
            let r = unsafe {
                CredUnPackAuthenticationBufferW(
                    flags,
                    out,
                    out_size,
                    PWSTR(user.as_mut_ptr()),
                    &mut ul,
                    PWSTR(domain.as_mut_ptr()),
                    Some(&mut dl),
                    PWSTR(password.as_mut_ptr()),
                    &mut pl,
                )
            };
            if r.is_ok() {
                unpacked = true;
                break;
            }
            zero_u16(&mut password);
        }
        // SAFETY: `out` holds `out_size` bytes from the prompt; zeroed, then freed once.
        unsafe {
            zero_bytes(std::slice::from_raw_parts_mut(out as *mut u8, out_size as usize));
            CoTaskMemFree(Some(out as *const std::ffi::c_void));
        }
        if !unpacked {
            zero_u16(&mut password);
            return None;
        }
        let name = String::from_utf16_lossy(until_nul(&user));
        let dom = String::from_utf16_lossy(until_nul(&domain));
        let secret = Secret::new(String::from_utf16_lossy(until_nul(&password)));
        zero_u16(&mut password);
        let name = name.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let user = if !dom.trim().is_empty() && !name.contains('\\') && !name.contains('@') { format!("{}\\{name}", dom.trim()) } else { name };
        Some(Credentials { user, password: secret })
    }

    pub fn pick_folder(current: &Path, title: &str) -> Option<PathBuf> {
        // SAFETY: COM was initialised on this thread by `prepare`; every
        // string outlives its call and the returned name is freed once.
        unsafe {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let options = dialog.GetOptions().ok()?;
            dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
            let t = wide(title);
            let _ = dialog.SetTitle(PCWSTR(t.as_ptr()));
            if let Some(start) = current.ancestors().find(|a| a.is_dir()) {
                let w = wide_path(start);
                if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(w.as_ptr()), None::<&IBindCtx>) {
                    let _ = dialog.SetFolder(&item);
                }
            }
            dialog.Show(HWND::default()).ok()?;
            let item = dialog.GetResult().ok()?;
            let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = name.to_string().ok();
            CoTaskMemFree(Some(name.0 as *const std::ffi::c_void));
            path.filter(|p| !p.is_empty()).map(PathBuf::from)
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::{Closed, Watch};
    use omni_deploy::texts::Lang;
    use omni_deploy::ui::{CredRequest, Credentials, Dialog, ID_CANCEL};
    use std::path::{Path, PathBuf};

    pub fn prepare() {}
    pub fn message_box(_title: &str, _text: &str, _error: bool) {}
    pub fn show(_d: &Dialog, _auto: bool, _watch: Option<Watch<'_>>, _lang: Lang) -> Closed {
        Closed { button: ID_CANCEL, radio: 0, checked: false }
    }
    pub fn credentials(_r: &CredRequest, _c: &str, _m: &str) -> Option<Credentials> {
        None
    }
    pub fn pick_folder(_current: &Path, _title: &str) -> Option<PathBuf> {
        None
    }
}
