//! Windows Event Log integration (plan P6.1, defect W-11).
//!
//! The rotated file in `logs/` is where the detail lives. This is for the other
//! audience: the IT administrator who did not install this daemon, does not
//! know where it keeps its files, and is looking at Event Viewer because
//! something on the machine is wrong. Three moments are worth a row there —
//! the service started, the service stopped, the service failed — and nothing
//! else, because a chatty source in the Application log is a source that gets
//! filtered out.
//!
//! Registration writes the keys Windows needs to render the messages. Without
//! them Event Viewer shows the text wrapped in a complaint about a missing
//! message resource, which is technically readable and looks broken. Pointing
//! `EventMessageFile` at the system's own `EventCreate.exe` is the standard way
//! to get plain-text events without shipping a compiled message DLL — the
//! alternative would mean a resource-only DLL beside the exe, which is a build
//! step this project does not have.

#[cfg(windows)]
use anyhow::{Context, Result};

/// Severity, mapped to the Event Viewer levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventLevel {
    Info,
    Warning,
    Error,
}

impl EventLevel {
    /// The `EVENTLOG_*_TYPE` constant.
    fn as_type(self) -> u16 {
        match self {
            EventLevel::Info => 0x0004,    // EVENTLOG_INFORMATION_TYPE
            EventLevel::Warning => 0x0002, // EVENTLOG_WARNING_TYPE
            EventLevel::Error => 0x0001,   // EVENTLOG_ERROR_TYPE
        }
    }

    /// Event id. Stable, because an administrator may build a filter on it.
    fn as_id(self) -> u32 {
        match self {
            EventLevel::Info => 1000,
            EventLevel::Warning => 2000,
            EventLevel::Error => 3000,
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::System::EventLog::{
        DeregisterEventSource, RegisterEventSourceW, ReportEventW, REPORT_EVENT_TYPE,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY,
        HKEY_LOCAL_MACHINE, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
    };

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn source_key_path() -> String {
        format!(
            r"SYSTEM\CurrentControlSet\Services\EventLog\Application\{}",
            crate::SERVICE_NAME
        )
    }

    /// Register the event source. Called at `service install`, which already
    /// requires an administrator.
    pub fn register_source() -> Result<()> {
        unsafe {
            let mut key = HKEY::default();
            let path = wide(&source_key_path());

            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                0,
                PWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            )
            .ok()
            .context("Failed creating the Event Log source key (needs administrator)")?;

            // `EventCreate.exe` carries a generic message resource that renders
            // a single inserted string as-is. Shipping our own message DLL
            // would mean a resource compile step; this gets readable events
            // with none.
            let message_file = wide(r"%SystemRoot%\System32\EventCreate.exe");
            let name = wide("EventMessageFile");
            let bytes = std::slice::from_raw_parts(
                message_file.as_ptr() as *const u8,
                message_file.len() * 2,
            );
            RegSetValueExW(key, PCWSTR(name.as_ptr()), 0, REG_EXPAND_SZ, Some(bytes))
                .ok()
                .context("Failed writing EventMessageFile")?;

            // Information | Warning | Error.
            let supported: u32 = 0x0007;
            let name = wide("TypesSupported");
            RegSetValueExW(
                key,
                PCWSTR(name.as_ptr()),
                0,
                REG_DWORD,
                Some(&supported.to_ne_bytes()),
            )
            .ok()
            .context("Failed writing TypesSupported")?;

            let _ = RegCloseKey(key);
        }
        Ok(())
    }

    /// Remove the event source at uninstall, so the machine is left as found.
    pub fn unregister_source() -> Result<()> {
        unsafe {
            let path = wide(&source_key_path());
            RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr()))
                .ok()
                .context("Failed removing the Event Log source key")?;
        }
        Ok(())
    }

    /// Write one event. Never fails the caller.
    pub fn report(level: EventLevel, message: &str) {
        unsafe {
            let source = wide(crate::SERVICE_NAME);
            let handle = match RegisterEventSourceW(PCWSTR::null(), PCWSTR(source.as_ptr())) {
                Ok(h) => h,
                Err(_) => return, // Not registered (not installed as a service).
            };

            // Event Viewer truncates long messages awkwardly; the file log is
            // where detail belongs.
            let truncated: String = message.chars().take(900).collect();
            let text = wide(&truncated);
            let strings = [PCWSTR(text.as_ptr())];

            let _ = ReportEventW(
                handle,
                REPORT_EVENT_TYPE(level.as_type()),
                0,
                level.as_id(),
                None,
                0,
                Some(&strings),
                None,
            );
            let _ = DeregisterEventSource(handle);
        }
    }
}

#[cfg(not(windows))]
mod windows_impl {
    use super::*;

    pub fn register_source() -> anyhow::Result<()> {
        Ok(())
    }
    pub fn unregister_source() -> anyhow::Result<()> {
        Ok(())
    }
    pub fn report(level: EventLevel, message: &str) {
        // Still visible in the file log on a non-Windows build.
        match level {
            EventLevel::Error => tracing::error!(target: "eventlog", "{message}"),
            EventLevel::Warning => tracing::warn!(target: "eventlog", "{message}"),
            EventLevel::Info => tracing::info!(target: "eventlog", "{message}"),
        }
    }
}

/// Register the event source (administrator required).
pub fn register_source() -> anyhow::Result<()> {
    windows_impl::register_source()
}

/// Remove the event source.
pub fn unregister_source() -> anyhow::Result<()> {
    windows_impl::unregister_source()
}

/// Write one event to the Application log.
///
/// Deliberately infallible: an Event Log that cannot be written is not a reason
/// to stop ingesting video, and the same message is already in the file log.
pub fn report(level: EventLevel, message: &str) {
    windows_impl::report(level, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_to_the_documented_event_viewer_constants() {
        // These are the values Event Viewer interprets; a wrong one shows an
        // error as information, which is worse than not logging it at all.
        assert_eq!(EventLevel::Error.as_type(), 0x0001);
        assert_eq!(EventLevel::Warning.as_type(), 0x0002);
        assert_eq!(EventLevel::Info.as_type(), 0x0004);
    }

    #[test]
    fn event_ids_are_distinct_and_stable() {
        // An administrator may filter on these, so they are part of the
        // interface and must not be renumbered casually.
        assert_eq!(EventLevel::Info.as_id(), 1000);
        assert_eq!(EventLevel::Warning.as_id(), 2000);
        assert_eq!(EventLevel::Error.as_id(), 3000);
    }

    #[test]
    fn reporting_without_a_registered_source_is_a_no_op_not_a_panic() {
        // The common case in development and in tests: not installed as a
        // service, so the source does not exist.
        report(EventLevel::Info, "test message from the unit test");
    }
}
