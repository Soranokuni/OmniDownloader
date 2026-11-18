//! The folder the payload is unpacked into: a new directory directly under
//! `%ProgramData%` (found through the known-folder API, not the environment),
//! created with an ACL that lets only Administrators and SYSTEM in from the
//! first moment. Never an existing directory, never a nested one (a parent
//! folder owned by someone else could be swapped under us). Removed at the end.

use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};

/// Protected (no inherited ACEs), Full Control for Administrators and SYSTEM only,
/// inherited by everything created inside: the installer's own descriptor.
pub const EXTRACT_SDDL: &str = omni_core::lockdown::PROTECTED_SDDL;

/// How many names are tried before giving up.
const ATTEMPTS: usize = 16;

/// `OmniIngest-Setup-<16 hex digits>`.
pub fn extract_dir_name(random: u64) -> String {
    format!("OmniIngest-Setup-{random:016x}")
}

/// 64 unpredictable bits: SipHash under the process's random keys, which std
/// takes from the operating system.
pub fn random_u64() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.write_u32(std::process::id());
    h.finish()
}

/// The unpack folder; removed when dropped if [`WorkDir::remove`] was not called.
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
    removed: bool,
}

impl WorkDir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the folder and everything in it. `Some(line)` says why it could not be.
    pub fn remove(&mut self) -> Option<String> {
        self.remove_with(&|p| std::fs::remove_dir_all(p))
    }

    /// [`WorkDir::remove`] with the remover given (tests inject one that
    /// fails). A folder that could not be removed is tried once more when
    /// dropped, silently: the caller has the line to show.
    pub fn remove_with(&mut self, remove: &dyn Fn(&Path) -> io::Result<()>) -> Option<String> {
        if self.removed {
            return None;
        }
        match remove(&self.path) {
            Ok(()) => {
                self.removed = true;
                None
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.removed = true;
                None
            }
            Err(e) => Some(format!("! Could not remove Setup's work folder {} ({e}); delete it by hand.", self.path.display())),
        }
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// A new folder under `parent`, created by `create` (which must fail with
/// `AlreadyExists` rather than adopt an existing folder).
pub fn create_work_dir(parent: &Path, create: &dyn Fn(&Path) -> io::Result<()>) -> io::Result<WorkDir> {
    for _ in 0..ATTEMPTS {
        let path = parent.join(extract_dir_name(random_u64()));
        match create(&path) {
            Ok(()) => return Ok(WorkDir { path, removed: false }),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name for Setup's work folder"))
}

/// The real folder: under `%ProgramData%`, with [`EXTRACT_SDDL`]
/// (`omni_core::lockdown::create_protected_dir`).
pub fn create_secure_work_dir() -> io::Result<WorkDir> {
    create_work_dir(&sys::program_data()?, &omni_core::lockdown::create_protected_dir)
}

#[cfg(windows)]
mod sys {
    use std::io;
    use std::path::PathBuf;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    fn io_err(e: windows::core::Error) -> io::Error {
        // HRESULT_FROM_WIN32 keeps the Win32 code in the low 16 bits.
        let code = e.code().0 as u32;
        if code & 0xFFFF_0000 == 0x8007_0000 {
            io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            io::Error::other(e.message())
        }
    }

    pub fn program_data() -> io::Result<PathBuf> {
        unsafe {
            let p = SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None).map_err(io_err)?;
            let path = p.to_string().map_err(|e| io::Error::other(e.to_string()));
            CoTaskMemFree(Some(p.0 as *const _));
            Ok(PathBuf::from(path?))
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use std::io;
    use std::path::PathBuf;

    pub fn program_data() -> io::Result<PathBuf> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "Setup runs on Windows only"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn the_work_folder_name_and_its_acl_are_fixed() {
        let name = extract_dir_name(0x0123_4567_89ab_cdef);
        assert_eq!(name, "OmniIngest-Setup-0123456789abcdef");
        assert_eq!(extract_dir_name(1), "OmniIngest-Setup-0000000000000001");
        // Protected, Administrators and SYSTEM only: no Users, no Everyone, no owner rights.
        assert_eq!(EXTRACT_SDDL, "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)");
        assert_ne!(random_u64(), random_u64());
    }

    #[test]
    fn an_existing_name_is_never_reused_and_the_folder_goes_at_the_end() {
        let d = tempfile::tempdir().unwrap();
        let tries = Cell::new(0);
        // The first name "exists": another is picked, never adopted.
        let create = |p: &Path| {
            tries.set(tries.get() + 1);
            if tries.get() == 1 {
                std::fs::create_dir(p)?;
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            std::fs::create_dir(p)
        };
        let mut w = create_work_dir(d.path(), &create).unwrap();
        assert_eq!(tries.get(), 2);
        assert!(w.path().parent() == Some(d.path()), "directly under the parent");
        std::fs::create_dir_all(w.path().join("bin")).unwrap();
        std::fs::write(w.path().join("bin").join("x.exe"), b"x").unwrap();
        let path = w.path().to_path_buf();
        assert_eq!(w.remove(), None);
        assert!(!path.exists());
        // The guard, on an early return.
        let path = {
            let w = create_work_dir(d.path(), &|p: &Path| std::fs::create_dir(p)).unwrap();
            w.path().to_path_buf()
        };
        assert!(!path.exists());
        // A creator that never succeeds gives up.
        assert!(create_work_dir(d.path(), &|_: &Path| Err(io::Error::from(io::ErrorKind::AlreadyExists))).is_err());
    }

    /// Y2: a folder that cannot be removed is said, and the drop still tries.
    #[test]
    fn a_work_folder_that_cannot_be_removed_is_said_and_the_drop_tries_again() {
        let d = tempfile::tempdir().unwrap();
        let mut w = create_work_dir(d.path(), &|p: &Path| std::fs::create_dir(p)).unwrap();
        let path = w.path().to_path_buf();
        let line = w.remove_with(&|_| Err(io::Error::from(io::ErrorKind::PermissionDenied))).unwrap();
        assert!(line.starts_with("! Could not remove Setup's work folder") && line.ends_with("delete it by hand."), "{line}");
        assert!(path.exists());
        drop(w);
        assert!(!path.exists(), "the safety net");
    }
}
