//! The Windows calls Setup makes outside the installer: the elevated relaunch,
//! opening the admin page in the operator's own browser, a delete at the next
//! restart, and the DLL search order. None of them is exercised by tests.

use std::path::Path;

/// How the elevated relaunch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relaunch {
    /// The child ran; its exit code.
    Exited(u32),
    /// The UAC prompt was declined.
    Declined,
}

/// Load every later DLL from System32 only (`SetDefaultDllDirectories`), never
/// from the folder Setup was downloaded to. Called first thing in `main`.
pub fn restrict_dll_search() {
    sys::restrict_dll_search()
}

/// Run `exe` with `parameters` elevated (the `runas` verb), wait for it, and
/// return its exit code. No working folder is passed: the elevated session may
/// not have the caller's mapped drive; paths in `parameters` are absolute.
pub fn relaunch_elevated(exe: &Path, parameters: &str) -> anyhow::Result<Relaunch> {
    sys::relaunch_elevated(exe, parameters)
}

/// Open `url` with the user's default browser, as this (unelevated) process.
pub fn open_url(url: &str) -> bool {
    sys::open_url(url)
}

/// Delete `path` (a file, or an empty folder) at the next restart.
pub fn delete_at_restart(path: &Path) -> std::io::Result<()> {
    sys::delete_at_restart(path)
}

#[cfg(windows)]
mod sys {
    use super::Relaunch;
    use anyhow::{Context, Result};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    use windows::Win32::System::LibraryLoader::{SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, ShellExecuteW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn restrict_dll_search() {
        // SAFETY: a process-wide flag, set before any other thread exists.
        let _ = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
    }

    pub fn relaunch_elevated(exe: &Path, parameters: &str) -> Result<Relaunch> {
        let verb = wide("runas".as_ref());
        let file = wide(exe.as_os_str());
        let params = wide(parameters.as_ref());
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // SAFETY: every string outlives the call; the process handle is closed once.
        unsafe {
            if let Err(e) = ShellExecuteExW(&mut info) {
                if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                    return Ok(Relaunch::Declined);
                }
                return Err(anyhow::Error::new(e).context("Could not start Setup with administrator rights"));
            }
            if info.hProcess.is_invalid() {
                anyhow::bail!("Setup was started with administrator rights but cannot be waited for");
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 0u32;
            let got = GetExitCodeProcess(info.hProcess, &mut code);
            let _ = CloseHandle(info.hProcess);
            got.context("Could not read the elevated Setup's exit code")?;
            Ok(Relaunch::Exited(code))
        }
    }

    pub fn open_url(url: &str) -> bool {
        let verb = wide("open".as_ref());
        let target = wide(url.as_ref());
        let rc = unsafe { ShellExecuteW(None, PCWSTR(verb.as_ptr()), PCWSTR(target.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
        rc.0 as isize > 32
    }

    pub fn delete_at_restart(path: &Path) -> std::io::Result<()> {
        let name = wide(path.as_os_str());
        unsafe { MoveFileExW(PCWSTR(name.as_ptr()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) }
            .map_err(|e| std::io::Error::other(e.message()))
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Relaunch;
    use std::path::Path;

    pub fn restrict_dll_search() {}

    pub fn relaunch_elevated(_exe: &Path, _parameters: &str) -> anyhow::Result<Relaunch> {
        anyhow::bail!("Setup runs on Windows only")
    }

    pub fn open_url(_url: &str) -> bool {
        false
    }

    pub fn delete_at_restart(_path: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "Setup runs on Windows only"))
    }
}
