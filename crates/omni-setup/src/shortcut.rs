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

/// Who an existing entry belongs to (P13.9b).
#[derive(Debug, PartialEq, Eq)]
pub enum Owner {
    /// No entry.
    Absent,
    /// It runs this installation's omni-ingest.exe.
    Ours,
    /// It runs another installation's; the target it names.
    Other(PathBuf),
    /// It could not be read; never touched.
    Unreadable(String),
}

/// What [`install`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Placed {
    Written(PathBuf),
    /// An entry of another installation, or one that cannot be read, is kept.
    Kept(Owner),
}

/// What [`remove`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Removed {
    Removed(PathBuf),
    /// No entry, or one that is not provably this installation's (kept).
    Kept(Owner),
}

/// Whose is the entry at `path`. The target is the path stored in the entry,
/// which stays readable after the program it names was deleted (uninstall
/// removes the files first).
#[cfg(windows)]
fn owner(path: &Path, dir: &Path) -> Owner {
    if !path.exists() {
        return Owner::Absent;
    }
    match os::read(path) {
        Ok((target, _, _)) if points_at(&target, dir) => Owner::Ours,
        Ok((target, _, _)) => Owner::Other(target),
        Err(e) => Owner::Unreadable(format!("{e:#}")),
    }
}

/// [`install`] into `folder` (tests use a temporary folder).
#[cfg(windows)]
pub fn install_in(folder: &Path, dir: &Path) -> Result<Placed, String> {
    let path = folder.join(SHORTCUT_FILE);
    match owner(&path, dir) {
        Owner::Absent | Owner::Ours => {
            os::write(&path, &spec(dir)).map_err(|e| format!("{e:#}"))?;
            Ok(Placed::Written(path))
        }
        kept => Ok(Placed::Kept(kept)),
    }
}

/// [`remove`] from `folder` (tests use a temporary folder): only an entry
/// that provably runs this installation.
#[cfg(windows)]
pub fn remove_in(folder: &Path, dir: &Path) -> Result<Removed, String> {
    let path = folder.join(SHORTCUT_FILE);
    match owner(&path, dir) {
        Owner::Ours => {
            std::fs::remove_file(&path).map_err(|e| format!("Cannot remove {}: {e}", path.display()))?;
            Ok(Removed::Removed(path))
        }
        kept => Ok(Removed::Kept(kept)),
    }
}

/// Write the entry for `dir` into the Start menu (all users).
pub fn install(dir: &Path) -> Result<Placed, String> {
    #[cfg(windows)]
    {
        install_in(&os::programs_dir().map_err(|e| format!("{e:#}"))?, dir)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        Err("Start-menu entries exist only on Windows".into())
    }
}

/// Remove the entry when it runs the installation in `dir`.
pub fn remove(dir: &Path) -> Result<Removed, String> {
    #[cfg(windows)]
    {
        remove_in(&os::programs_dir().map_err(|e| format!("{e:#}"))?, dir)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        Ok(Removed::Kept(Owner::Absent))
    }
}

fn kept_line(owner: &Owner) -> Option<String> {
    match owner {
        Owner::Absent | Owner::Ours => None,
        Owner::Other(target) => Some(format!("  The Start-menu sign-in entry runs {}; it is kept.", target.display())),
        Owner::Unreadable(why) => Some(format!("! The Start-menu sign-in entry could not be read ({why}); it is kept.")),
    }
}

/// The setup.log line for what [`install`] did.
pub fn install_line(done: &Result<Placed, String>) -> String {
    match done {
        Ok(Placed::Written(path)) => format!("  Start menu: {}", path.display()),
        Ok(Placed::Kept(owner)) => kept_line(owner).unwrap_or_default(),
        Err(e) => format!("! The Start-menu entry for site sign-in was not written ({e}); run `omni-ingest cookies sign-in` from an Administrator prompt instead."),
    }
}

/// The setup.log line for what [`remove`] did; none when nothing is worth saying.
pub fn remove_line(done: &Result<Removed, String>) -> Option<String> {
    match done {
        Ok(Removed::Removed(_)) => None,
        Ok(Removed::Kept(owner)) => kept_line(owner),
        Err(e) => Some(format!("! The Start-menu sign-in entry was not removed ({e}).")),
    }
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
        assert!(matches!(remove_in(tmp.path(), &other).unwrap(), Removed::Kept(Owner::Other(_))));
        assert!(path.exists(), "another installation's entry is kept");
        // P13.9b: uninstall removes the files first; the stored target still names them.
        std::fs::remove_dir_all(&ours).unwrap();
        assert!(points_at(&read(&path).unwrap().0, &ours), "the target is read with the program gone");
        assert_eq!(remove_in(tmp.path(), &ours).unwrap(), Removed::Removed(path.clone()));
        assert!(!path.exists());
        assert_eq!(remove_in(tmp.path(), &ours).unwrap(), Removed::Kept(Owner::Absent));
    }

    /// P13.9b: install keeps another installation's entry, and an entry that
    /// cannot be read is never touched; both are logged.
    #[cfg(windows)]
    #[test]
    fn another_installations_entry_and_an_unreadable_one_are_kept_and_logged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (ours, other) = (tmp.path().join("ours"), tmp.path().join("other"));
        let path = tmp.path().join(SHORTCUT_FILE);
        write(&path, &spec(&other)).unwrap();

        let placed = install_in(tmp.path(), &ours);
        assert!(matches!(placed, Ok(Placed::Kept(Owner::Other(_)))), "{placed:?}");
        assert!(points_at(&read(&path).unwrap().0, &other), "the other installation's entry is unchanged");
        assert!(install_line(&placed).contains("it is kept"), "{}", install_line(&placed));

        // Our own entry is rewritten in place.
        std::fs::remove_file(&path).unwrap();
        write(&path, &spec(&ours)).unwrap();
        assert_eq!(install_in(tmp.path(), &ours).unwrap(), Placed::Written(path.clone()));

        // Not a shortcut at all: never removed, never overwritten.
        std::fs::write(&path, b"not a shortcut").unwrap();
        let removed = remove_in(tmp.path(), &ours);
        assert!(matches!(removed, Ok(Removed::Kept(Owner::Unreadable(_)))), "{removed:?}");
        assert!(remove_line(&removed).unwrap().contains("could not be read"));
        assert!(matches!(install_in(tmp.path(), &ours), Ok(Placed::Kept(Owner::Unreadable(_)))));
        assert_eq!(std::fs::read(&path).unwrap(), b"not a shortcut");
        assert_eq!(remove_line(&Ok(Removed::Removed(path.clone()))), None);
    }
}
