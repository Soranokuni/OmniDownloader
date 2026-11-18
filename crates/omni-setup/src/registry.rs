//! The Apps & Features entry: `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\OmniIngest`.

use std::path::Path;

pub const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\OmniIngest";

/// The stub's name in the install folder.
pub const SETUP_EXE: &str = "OmniIngest-Setup.exe";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    Sz(String),
    Dword(u32),
}

/// The values of the entry for an installation in `dir`, in the order written.
pub fn uninstall_values(dir: &Path, version: &str, install_date: &str) -> Vec<(&'static str, RegValue)> {
    let setup = format!("\"{}\"", dir.join(SETUP_EXE).display());
    let sz = |s: String| RegValue::Sz(s);
    vec![
        ("DisplayName", sz("OmniIngest".into())),
        ("DisplayVersion", sz(version.into())),
        ("Publisher", sz("OmniIngest".into())),
        ("InstallLocation", sz(dir.display().to_string())),
        ("DisplayIcon", sz(dir.join("omni-ingest.exe").display().to_string())),
        ("UninstallString", sz(format!("{setup} /uninstall"))),
        ("QuietUninstallString", sz(format!("{setup} /uninstall /quiet"))),
        ("ModifyPath", sz(format!("{setup} /change-account"))),
        ("NoRepair", RegValue::Dword(1)),
        ("InstallDate", sz(install_date.into())),
    ]
}

/// Write (replace) the entry. Administrator only.
pub fn write_entry(values: &[(&'static str, RegValue)]) -> anyhow::Result<()> {
    sys::write(values)
}

/// Remove the entry; a missing one is fine.
pub fn remove_entry() -> anyhow::Result<()> {
    sys::remove()
}

/// The entry's `InstallLocation`; `None` when the entry or the value is missing.
pub fn read_install_location() -> Option<std::path::PathBuf> {
    sys::read_value("InstallLocation").map(std::path::PathBuf::from)
}

/// The entry's `DisplayVersion` (the version Setup installed); `None` when missing.
pub fn read_display_version() -> Option<String> {
    sys::read_value("DisplayVersion")
}

#[cfg(windows)]
mod sys {
    use super::{RegValue, UNINSTALL_KEY};
    use anyhow::{Context, Result};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY,
        KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn write(values: &[(&'static str, RegValue)]) -> Result<()> {
        let path = wide(UNINSTALL_KEY);
        unsafe {
            let mut key = HKEY::default();
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                0,
                PWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE | KEY_WOW64_64KEY,
                None,
                &mut key,
                None,
            )
            .ok()
            .context("Cannot create the Apps & Features entry (needs administrator)")?;
            let mut result = Ok(());
            for (name, value) in values {
                let n = wide(name);
                let rc = match value {
                    RegValue::Sz(s) => {
                        let w = wide(s);
                        let bytes = std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2);
                        RegSetValueExW(key, PCWSTR(n.as_ptr()), 0, REG_SZ, Some(bytes))
                    }
                    RegValue::Dword(d) => RegSetValueExW(key, PCWSTR(n.as_ptr()), 0, REG_DWORD, Some(&d.to_ne_bytes())),
                };
                if let Err(e) = rc.ok() {
                    result = Err(anyhow::Error::new(e).context(format!("Cannot write {name} in the Apps & Features entry")));
                    break;
                }
            }
            let _ = RegCloseKey(key);
            result
        }
    }

    pub fn read_value(value: &str) -> Option<String> {
        use windows::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY};
        let path = wide(UNINSTALL_KEY);
        let name = wide(value);
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: the buffer and its size in bytes are passed together.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                None,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                Some(&mut bytes),
            )
        };
        if rc.is_err() {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..end]);
        (!s.trim().is_empty()).then_some(s)
    }

    pub fn remove() -> Result<()> {
        let path = wide(UNINSTALL_KEY);
        let rc = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr())) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        rc.ok().context("Cannot remove the Apps & Features entry")
    }
}

#[cfg(not(windows))]
mod sys {
    use super::RegValue;

    pub fn write(_values: &[(&'static str, RegValue)]) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn remove() -> anyhow::Result<()> {
        Ok(())
    }

    pub fn read_value(_value: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_apps_and_features_entry_is_exactly_this() {
        let dir = Path::new(r"C:\Program Files\Omni Ingest");
        let got = uninstall_values(dir, "1.0.0", "20261117");
        let sz = |s: &str| RegValue::Sz(s.to_string());
        let want = vec![
            ("DisplayName", sz("OmniIngest")),
            ("DisplayVersion", sz("1.0.0")),
            ("Publisher", sz("OmniIngest")),
            ("InstallLocation", sz(r"C:\Program Files\Omni Ingest")),
            ("DisplayIcon", sz(r"C:\Program Files\Omni Ingest\omni-ingest.exe")),
            ("UninstallString", sz(r#""C:\Program Files\Omni Ingest\OmniIngest-Setup.exe" /uninstall"#)),
            ("QuietUninstallString", sz(r#""C:\Program Files\Omni Ingest\OmniIngest-Setup.exe" /uninstall /quiet"#)),
            ("ModifyPath", sz(r#""C:\Program Files\Omni Ingest\OmniIngest-Setup.exe" /change-account"#)),
            ("NoRepair", RegValue::Dword(1)),
            ("InstallDate", sz("20261117")),
        ];
        assert_eq!(got, want);
        assert!(got.iter().all(|(n, _)| *n != "NoModify"));
    }
}
