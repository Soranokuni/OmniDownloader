//! The binary's console (P13.1c). Setup is a windowed program, so a
//! double-click shows no console; the modes that print (`/quiet`, `--pack`,
//! `--verify`, `/?`, a usage error) attach to the console of whoever started
//! them. A handle that is already valid (a redirect to a file or a pipe) is
//! never replaced.

/// Attach to the parent's console and point stdout and stderr at it where
/// they lead nowhere. Whether anything printed now reaches someone: a
/// console, or a redirected stdout.
pub fn attach() -> bool {
    sys::attach()
}

#[cfg(windows)]
mod sys {
    use windows::core::w;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileType, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_UNKNOWN, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// A handle this process can write to.
    fn usable(which: STD_HANDLE) -> bool {
        // SAFETY: plain queries of this process's standard handles.
        unsafe {
            match GetStdHandle(which) {
                Ok(h) if !h.is_invalid() => GetFileType(h) != FILE_TYPE_UNKNOWN,
                _ => false,
            }
        }
    }

    pub fn attach() -> bool {
        // SAFETY: attaching fails harmlessly when there is no parent console
        // or one is attached already; the console output handle is opened
        // by its documented name and kept for the life of the process.
        unsafe {
            let attached = AttachConsole(ATTACH_PARENT_PROCESS).is_ok();
            if attached {
                for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                    if usable(which) {
                        continue;
                    }
                    if let Ok(h) = CreateFileW(
                        w!("CONOUT$"),
                        (GENERIC_READ | GENERIC_WRITE).0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAGS_AND_ATTRIBUTES(0),
                        HANDLE::default(),
                    ) {
                        let _ = SetStdHandle(which, h);
                    }
                }
            }
            attached || usable(STD_OUTPUT_HANDLE)
        }
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn attach() -> bool {
        true
    }
}
