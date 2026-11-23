//! The Start-menu entry «OmniIngest – Σύνδεση σε ιστότοπο» (P13.9): signs a
//! site in without a terminal. It runs `omni-ingest.exe cookies sign-in` as
//! administrator (the shortcut's "Run as administrator" flag); with no site
//! named, the command shows a numbered list of sites to choose from.
//!
//! Setup writes it for all users after an install or upgrade and removes it
//! when it uninstalls the installation the entry points at; an entry that
//! points at another installation is left alone.

use std::path::{Path, PathBuf};

/// The entry's file name in the Start menu's Programs folder.
pub const SHORTCUT_FILE: &str = "OmniIngest – Σύνδεση σε ιστότοπο.lnk";

/// What the entry runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutSpec {
    pub target: PathBuf,
    pub arguments: String,
    pub working_dir: PathBuf,
    pub description: String,
    /// "Run as administrator": the sign-in writes the sealed cookie store.
    pub run_as_admin: bool,
}

/// The entry for the installation in `dir`.
pub fn spec(dir: &Path) -> ShortcutSpec {
    ShortcutSpec {
        target: dir.join("omni-ingest.exe"),
        arguments: "cookies sign-in".into(),
        working_dir: dir.to_path_buf(),
        description: "Σύνδεση του λογαριασμού του σταθμού σε ιστότοπο (YouTube, X, Facebook, Instagram) για λήψεις που θέλουν σύνδεση".into(),
        run_as_admin: true,
    }
}

/// Whether an entry whose target is `target` belongs to the installation in `dir`.
pub fn points_at(target: &Path, dir: &Path) -> bool {
    let a = target.to_string_lossy().to_lowercase();
    let b = dir.join("omni-ingest.exe").to_string_lossy().to_lowercase();
    a.trim_end_matches('\\') == b.trim_end_matches('\\')
}

#[cfg(windows)]
mod os {
    use super::*;
    use anyhow::{Context, Result};
    use windows::core::{Interface, HSTRING, PCWSTR};
    use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::{
        FOLDERID_CommonPrograms, IShellLinkDataList, IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT,
        SLDF_RUNAS_USER,
    };

    /// Run `f` on a thread of its own with COM initialised, whatever the caller's apartment.
    fn with_com<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
        std::thread::spawn(move || {
            // SAFETY: initialised and uninitialised on this thread only.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().context("COM could not start")? };
            let out = f();
            unsafe { CoUninitialize() };
            out
        })
        .join()
        .map_err(|_| anyhow::anyhow!("the shortcut thread stopped"))?
    }

    /// The all-users Start menu Programs folder.
    pub fn programs_dir() -> Result<PathBuf> {
        // SAFETY: the returned buffer is freed with CoTaskMemFree.
        unsafe {
            let p = SHGetKnownFolderPath(&FOLDERID_CommonPrograms, KF_FLAG_DEFAULT, None).context("The Start menu folder is unknown")?;
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            Ok(PathBuf::from(s))
        }
    }

    pub fn write(path: &Path, spec: &ShortcutSpec) -> Result<()> {
        let (path, spec) = (path.to_path_buf(), spec.clone());
        with_com(move || unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).context("ShellLink")?;
            link.SetPath(&HSTRING::from(spec.target.as_os_str()))?;
            link.SetArguments(&HSTRING::from(spec.arguments.as_str()))?;
            link.SetWorkingDirectory(&HSTRING::from(spec.working_dir.as_os_str()))?;
            link.SetDescription(&HSTRING::from(spec.description.as_str()))?;
            link.SetIconLocation(&HSTRING::from(spec.target.as_os_str()), 0)?;
            if spec.run_as_admin {
                let data: IShellLinkDataList = link.cast()?;
                let flags = data.GetFlags()?;
                data.SetFlags(flags | SLDF_RUNAS_USER.0 as u32)?;
            }
            let file: IPersistFile = link.cast()?;
            file.Save(&HSTRING::from(path.as_os_str()), true).with_context(|| format!("Cannot write {}", path.display()))?;
            Ok(())
        })
    }

    /// The target and the "Run as administrator" flag of the entry at `path`.
    pub fn read(path: &Path) -> Result<(PathBuf, String, bool)> {
        let path = path.to_path_buf();
        with_com(move || unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).context("ShellLink")?;
            let file: IPersistFile = link.cast()?;
            file.Load(PCWSTR(HSTRING::from(path.as_os_str()).as_ptr()), STGM_READ)?;
            let mut target = [0u16; 1024];
            let mut find = WIN32_FIND_DATAW::default();
            link.GetPath(&mut target, &mut find, 0)?;
            let mut args = [0u16; 1024];
            link.GetArguments(&mut args)?;
            let text = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|c| *c == 0).unwrap_or(b.len())]);
            let data: IShellLinkDataList = link.cast()?;
            let runas = data.GetFlags()? & SLDF_RUNAS_USER.0 as u32 != 0;
            Ok((PathBuf::from(text(&target)), text(&args), runas))
        })
    }
}

/// Write the entry for `dir` into the Start menu; a line for setup.log when it could not.
pub fn install(dir: &Path) -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        let folder = os::programs_dir().map_err(|e| format!("{e:#}"))?;
        let path = folder.join(SHORTCUT_FILE);
        os::write(&path, &spec(dir)).map_err(|e| format!("{e:#}"))?;
        Ok(path)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        Err("Start-menu entries exist only on Windows".into())
    }
}

/// What [`remove`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Removed {
    Removed(PathBuf),
    /// No entry.
    Absent,
    /// The entry points at another installation; kept.
    Other(PathBuf),
}

/// Remove the entry when it points at the installation in `dir`.
pub fn remove(dir: &Path) -> Result<Removed, String> {
    #[cfg(windows)]
    {
        let folder = os::programs_dir().map_err(|e| format!("{e:#}"))?;
        remove_in(&folder, dir)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        Ok(Removed::Absent)
    }
}

/// [`remove`] for the entry in `folder` (tests use a temporary folder).
#[cfg(windows)]
pub fn remove_in(folder: &Path, dir: &Path) -> Result<Removed, String> {
    let path = folder.join(SHORTCUT_FILE);
    if !path.exists() {
        return Ok(Removed::Absent);
    }
    // An entry that cannot be read is ours by name: it is removed.
    let target = os::read(&path).map(|(t, _, _)| t).unwrap_or_else(|_| dir.join("omni-ingest.exe"));
    if !points_at(&target, dir) {
        return Ok(Removed::Other(target));
    }
    std::fs::remove_file(&path).map_err(|e| format!("Cannot remove {}: {e}", path.display()))?;
    Ok(Removed::Removed(path))
}

#[cfg(windows)]
pub use os::{read, write};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_runs_the_sign_in_of_its_own_installation_as_administrator() {
        let dir = Path::new(r"C:\Program Files\Omni Ingest");
        let s = spec(dir);
        assert_eq!(s.target, dir.join("omni-ingest.exe"));
        assert_eq!((s.arguments.as_str(), s.run_as_admin), ("cookies sign-in", true));
        assert!(points_at(Path::new(r"c:\program files\omni ingest\OMNI-INGEST.EXE"), dir));
        assert!(!points_at(Path::new(r"D:\Other\omni-ingest.exe"), dir));
        assert_eq!(SHORTCUT_FILE, "OmniIngest – Σύνδεση σε ιστότοπο.lnk");
    }

    /// The real .lnk, written to a temporary folder (no Start menu, no elevation).
    #[cfg(windows)]
    #[test]
    fn a_written_entry_reads_back_with_its_target_and_the_administrator_flag_and_is_removed_only_when_ours() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ours = tmp.path().join("install");
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("omni-ingest.exe"), b"MZ").unwrap();
        let path = tmp.path().join(SHORTCUT_FILE);
        write(&path, &spec(&ours)).unwrap();
        let (target, args, runas) = read(&path).unwrap();
        assert!(points_at(&target, &ours), "{}", target.display());
        assert_eq!((args.as_str(), runas), ("cookies sign-in", true));

        let other = tmp.path().join("other-install");
        assert!(matches!(remove_in(tmp.path(), &other).unwrap(), Removed::Other(_)));
        assert!(path.exists(), "another installation's entry is kept");
        assert_eq!(remove_in(tmp.path(), &ours).unwrap(), Removed::Removed(path.clone()));
        assert!(!path.exists());
        assert_eq!(remove_in(tmp.path(), &ours).unwrap(), Removed::Absent);
    }
}
