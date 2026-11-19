//! Lock down `data\` and `temp\` on an installation (plan P3.5).
//!
//! `data\` holds the database, the sealed secret store and the sealed cookie
//! jars; `temp\` holds a jar in clear for the length of one yt-dlp run. Both
//! inherit their ACL from the install directory, which other local users may be
//! able to read. This removes the inheritance and grants only SYSTEM,
//! Administrators and the account the service runs as.
//!
//! Only ever run for the Windows service and the installer, never in console
//! (development) mode: it would lock the developer's own account out.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::info;

const SYSTEM_SID: &str = "S-1-5-18";
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
const TRUSTED_INSTALLER_SID: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// `icacls.exe` by its full path: the installer runs elevated, and a bare name
/// is looked up beside the running executable first. An error when the system
/// directory cannot be found: never a bare name.
pub fn icacls() -> std::io::Result<PathBuf> {
    crate::paths::system_tool("icacls.exe")
}

/// `whoami.exe` by its full path (see [`icacls`]).
pub fn whoami() -> std::io::Result<PathBuf> {
    crate::paths::system_tool("whoami.exe")
}

/// Protected (nothing inherited), Full Control for Administrators and SYSTEM
/// only, inherited by everything created inside: Setup's work folder.
pub const PROTECTED_SDDL: &str = "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)";

/// A new install root and its missing parents (P2.16 baseline): protected,
/// Administrators and SYSTEM Full, Users Read & execute, all inherited. The
/// service account can run the executable even when the lockdown step is
/// skipped or fails; it gets its write rights per folder afterwards.
pub const INSTALL_ROOT_SDDL: &str = "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)";

/// Create the directory `path` (its parent must exist) with [`PROTECTED_SDDL`]
/// from the first moment, so nobody else can plant anything in it before the
/// installer sets its final rights. Fails with `AlreadyExists` rather than
/// adopt an existing directory.
pub fn create_protected_dir(path: &Path) -> std::io::Result<()> {
    sys::create_dir_with_sddl(path, PROTECTED_SDDL)
}

/// [`create_protected_dir`] with [`INSTALL_ROOT_SDDL`]: a new install root or one of its parents.
pub fn create_install_root_dir(path: &Path) -> std::io::Result<()> {
    sys::create_dir_with_sddl(path, INSTALL_ROOT_SDDL)
}

/// The owners an install folder may have: Administrators, SYSTEM and
/// TrustedInstaller. Not the user running the installer: a folder that user
/// owns was made unelevated, and its owner keeps WRITE_DAC whatever is set.
pub fn is_trusted_owner(sid: &str) -> bool {
    let s = sid.trim();
    s.eq_ignore_ascii_case(ADMINISTRATORS_SID) || s.eq_ignore_ascii_case(SYSTEM_SID) || s.eq_ignore_ascii_case(TRUSTED_INSTALLER_SID)
}

/// The `S-1-…` of an existing folder's owner; `None` when it cannot be read.
pub fn owner_sid(dir: &Path) -> Option<String> {
    sys::owner(dir).map(|(sid, _)| sid)
}

/// For an existing folder, the owner's name when it is not a trusted owner
/// ([`is_trusted_owner`]): an owner can always rewrite the folder's rights,
/// whatever the installer set. `None` when the folder does not exist or its
/// owner is trusted. An owner that cannot be read is not trusted.
pub fn foreign_owner(dir: &Path) -> Option<String> {
    std::fs::symlink_metadata(dir).ok()?;
    match sys::owner(dir) {
        Some((sid, _)) if is_trusted_owner(&sid) => None,
        Some((_, name)) => Some(name),
        None => Some("an account whose ownership Setup cannot read".into()),
    }
}

/// One entry of a folder's DACL, as [`swap_principal`] judges it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ace {
    pub sid: String,
    pub mask: u32,
    /// `INHERIT_ONLY_ACE` (0x08) and the other ACE header flags.
    pub flags: u8,
    /// An allow entry; deny entries are kept but never make a folder swappable.
    pub allow: bool,
    pub name: String,
}

/// The entries of `dir`'s DACL. A missing DACL (everyone has full control) is
/// one Everyone/GENERIC_ALL entry.
pub fn read_dacl(dir: &Path) -> std::io::Result<Vec<Ace>> {
    sys::read_dacl(dir)
}

const INHERIT_ONLY_ACE: u8 = 0x08;
/// DELETE, WRITE_DAC, WRITE_OWNER, FILE_DELETE_CHILD and GENERIC_ALL: what lets a
/// principal rename, replace or re-permission a folder (and so swap what is in it).
const SWAP_RIGHTS: u32 = 0x0001_0000 | 0x0004_0000 | 0x0008_0000 | 0x0000_0040 | 0x1000_0000;

/// Who, besides Administrators, SYSTEM and TrustedInstaller, may rename or
/// re-permission a folder with these entries: the first allow entry that
/// applies to the folder itself (not inherit-only) with a swap right. Pure.
pub fn swap_principal(aces: &[Ace]) -> Option<String> {
    aces.iter()
        .filter(|a| a.allow && a.flags & INHERIT_ONLY_ACE == 0 && a.mask & SWAP_RIGHTS != 0)
        .find(|a| !is_trusted_owner(&a.sid))
        .map(|a| if a.name.is_empty() { a.sid.clone() } else { a.name.clone() })
}

/// FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_DELETE_CHILD, DELETE, WRITE_DAC,
/// WRITE_OWNER, GENERIC_WRITE and GENERIC_ALL: what lets a principal put
/// something into a folder, or take it over.
const WRITE_RIGHTS: u32 = 0x0000_0002 | 0x0000_0004 | 0x0000_0040 | 0x0001_0000 | 0x0004_0000 | 0x0008_0000 | 0x4000_0000 | 0x1000_0000;
const CREATOR_OWNER_SID: &str = "S-1-3-0";

/// Who, besides Administrators, SYSTEM, TrustedInstaller and CREATOR OWNER,
/// may write into a folder with these entries, inherit-only entries included
/// (they apply to the `bin` and `data` an installer is about to create). Pure.
pub fn write_principal(aces: &[Ace]) -> Option<String> {
    aces.iter()
        .filter(|a| a.allow && a.mask & WRITE_RIGHTS != 0)
        .find(|a| !is_trusted_owner(&a.sid) && !a.sid.eq_ignore_ascii_case(CREATOR_OWNER_SID))
        .map(|a| if a.name.is_empty() { a.sid.clone() } else { a.name.clone() })
}

/// [`write_principal`] of an existing folder; a DACL that cannot be read counts as writable.
pub fn writable_by(dir: &Path) -> Option<String> {
    match read_dacl(dir) {
        Ok(aces) => write_principal(&aces),
        Err(_) => Some("an account Setup cannot read".into()),
    }
}

/// How an ACE type is read: `Some(true)` allow (ACCESS_ALLOWED 0, and the
/// conditional ACCESS_ALLOWED_CALLBACK 9, whose condition is not evaluated: it
/// may hold), `Some(false)` deny (1 and its callback form 10), `None` for the
/// object and audit kinds, which have another layout and are skipped.
pub fn ace_kind(ace_type: u8) -> Option<bool> {
    match ace_type {
        0 | 9 => Some(true),
        1 | 10 => Some(false),
        _ => None,
    }
}

/// Who, besides Administrators, SYSTEM and TrustedInstaller, may swap a folder:
/// its owner when that is not trusted (an owner always has WRITE_DAC), else
/// [`swap_principal`] of its entries. `owner` is (`S-1-…`, name). Pure.
pub fn swap_reason(owner: Option<(&str, &str)>, aces: &[Ace]) -> Option<String> {
    match owner {
        Some((sid, _)) if is_trusted_owner(sid) => swap_principal(aces),
        Some((_, name)) => Some(format!("{name} (its owner)")),
        None => Some("an account whose ownership Setup cannot read".into()),
    }
}

/// [`swap_reason`] of an existing folder; `None` for a missing one. A DACL that
/// cannot be read counts as swappable.
pub fn folder_swapper(dir: &Path) -> Option<String> {
    std::fs::symlink_metadata(dir).ok()?;
    let owner = sys::owner(dir);
    match read_dacl(dir) {
        Ok(aces) => swap_reason(owner.as_ref().map(|(s, n)| (s.as_str(), n.as_str())), &aces),
        Err(_) => Some("an account Setup cannot read".into()),
    }
}

/// Whether Windows knows `name` (`DOMAIN\user`, `.\user`, `user@domain`).
/// When it cannot tell (no domain controller), it says yes: the registration
/// then reports the real error.
pub fn account_exists(name: &str) -> bool {
    sys::account_exists(name)
}

#[cfg(windows)]
mod sys {
    use super::Ace;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::{HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, ERROR_NONE_MAPPED, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetAce, LookupAccountNameW, LookupAccountSidW, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SID_NAME_USE,
    };
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn io_err(e: windows::core::Error) -> io::Error {
        let code = e.code().0 as u32;
        if code & 0xFFFF_0000 == 0x8007_0000 {
            io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            io::Error::other(e.message())
        }
    }

    pub fn create_dir_with_sddl(path: &Path, sddl: &str) -> io::Result<()> {
        let sddl = wide(sddl.as_ref());
        let name = wide(path.as_os_str());
        // SAFETY: the buffers outlive the calls; the descriptor is freed once.
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None).map_err(io_err)?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let made = CreateDirectoryW(PCWSTR(name.as_ptr()), Some(&sa));
            let _ = LocalFree(HLOCAL(sd.0));
            made.map_err(io_err)
        }
    }

    /// `S-1-…` of a SID.
    unsafe fn sid_string(sid: PSID) -> Option<String> {
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(sid, &mut s).ok()?;
        let text = s.to_string().ok();
        let _ = LocalFree(HLOCAL(s.0 as _));
        text
    }

    /// `DOMAIN\name` of a SID, or its `S-1-…` form.
    unsafe fn sid_name(sid: PSID) -> String {
        let (mut name, mut domain) = (vec![0u16; 256], vec![0u16; 256]);
        let (mut n, mut d) = (name.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        if LookupAccountSidW(PCWSTR::null(), sid, PWSTR(name.as_mut_ptr()), &mut n, PWSTR(domain.as_mut_ptr()), &mut d, &mut kind).is_ok() {
            let name = String::from_utf16_lossy(&name[..n as usize]);
            let domain = String::from_utf16_lossy(&domain[..d as usize]);
            return if domain.is_empty() { name } else { format!(r"{domain}\{name}") };
        }
        sid_string(sid).unwrap_or_else(|| "an unknown account".into())
    }

    /// The owner of `dir` as (`S-1-…`, name); `None` when it cannot be read.
    pub fn owner(dir: &Path) -> Option<(String, String)> {
        let name = wide(dir.as_os_str());
        // SAFETY: the owner SID points into `sd`, which is freed after its last use.
        unsafe {
            let mut owner = PSID::default();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let rc = GetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                None,
                None,
                &mut sd,
            );
            if rc.is_err() {
                return None;
            }
            let result = if owner.is_invalid() { None } else { sid_string(owner).map(|s| (s, sid_name(owner))) };
            let _ = LocalFree(HLOCAL(sd.0));
            result
        }
    }

    pub fn read_dacl(dir: &Path) -> io::Result<Vec<Ace>> {
        let name = wide(dir.as_os_str());
        // SAFETY: the ACL and every ACE point into `sd`, freed after the last use;
        // each ACE is read through its header before its body.
        unsafe {
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let rc = GetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut sd,
            );
            if rc.is_err() {
                return Err(io::Error::from_raw_os_error(rc.0 as i32));
            }
            let mut aces = Vec::new();
            if dacl.is_null() {
                aces.push(Ace { sid: "S-1-1-0".into(), mask: 0x1000_0000, flags: 0, allow: true, name: "Everyone".into() });
            } else {
                for i in 0..(*dacl).AceCount as u32 {
                    let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
                    if GetAce(dacl, i, &mut p).is_err() || p.is_null() {
                        continue;
                    }
                    let header = &*(p as *const ACE_HEADER);
                    // Every kind read here has the ACCESS_ALLOWED_ACE layout up to the SID.
                    let Some(allow) = super::ace_kind(header.AceType) else {
                        continue;
                    };
                    let ace = &*(p as *const ACCESS_ALLOWED_ACE);
                    let sid = PSID(&ace.SidStart as *const u32 as *mut _);
                    aces.push(Ace {
                        sid: sid_string(sid).unwrap_or_default(),
                        mask: ace.Mask,
                        flags: header.AceFlags,
                        allow,
                        name: sid_name(sid),
                    });
                }
            }
            let _ = LocalFree(HLOCAL(sd.0));
            Ok(aces)
        }
    }

    pub fn account_exists(name: &str) -> bool {
        let n = name.trim();
        let n = n.strip_prefix(r".\").unwrap_or(n);
        let w = wide(n.as_ref());
        let (mut sid_len, mut domain_len) = (0u32, 0u32);
        let mut kind = SID_NAME_USE::default();
        // SAFETY: a size query: no buffer is written with zero lengths.
        let r = unsafe {
            LookupAccountNameW(PCWSTR::null(), PCWSTR(w.as_ptr()), PSID::default(), &mut sid_len, PWSTR::null(), &mut domain_len, &mut kind)
        };
        match r {
            Ok(()) => true,
            Err(e) => e.code() != HRESULT::from_win32(ERROR_NONE_MAPPED.0),
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Ace;
    use std::path::Path;

    pub fn create_dir_with_sddl(path: &Path, _sddl: &str) -> std::io::Result<()> {
        std::fs::create_dir(path)
    }

    pub fn owner(_dir: &Path) -> Option<(String, String)> {
        Some(("S-1-5-32-544".into(), "Administrators".into()))
    }

    pub fn read_dacl(_dir: &Path) -> std::io::Result<Vec<Ace>> {
        Ok(Vec::new())
    }

    pub fn account_exists(_name: &str) -> bool {
        true
    }
}

/// Which account besides SYSTEM and Administrators keeps access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    /// The service runs as LocalSystem: nothing to add.
    System,
    /// A named account (`DOMAIN\user`, `NT SERVICE\Name`) or a SID (`S-1-5-...`).
    Named(String),
    /// The account this process runs as, asked of Windows. If it cannot be
    /// found nothing is changed: guessing wrong would lock the service out.
    Current,
}

/// `*SID` for a SID, the name as given otherwise: how icacls takes an account.
pub fn account_spec(account: &str) -> String {
    let a = account.trim();
    if a.starts_with("S-1-") {
        format!("*{a}")
    } else {
        a.to_string()
    }
}

/// Whether `account` is LocalSystem, which is granted already.
fn is_system(account: &str) -> bool {
    let a = account.trim().to_ascii_uppercase();
    a == SYSTEM_SID || a == "LOCALSYSTEM" || a == "NT AUTHORITY\\SYSTEM" || a == "NT AUTHORITY\\LOCALSYSTEM"
}

/// The icacls arguments for one directory: inheritance removed, SYSTEM and
/// Administrators Full, the service account Modify when it is not SYSTEM.
/// Pure. `/grant:r` replaces an existing entry, so running it twice changes nothing.
pub fn lockdown_args(dir: &Path, service_account: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        dir.as_os_str().to_os_string(),
        "/inheritance:r".into(),
        "/grant:r".into(),
        format!("*{SYSTEM_SID}:(OI)(CI)F").into(),
        "/grant:r".into(),
        format!("*{ADMINISTRATORS_SID}:(OI)(CI)F").into(),
    ];
    if let Some(a) = service_account.filter(|a| !a.trim().is_empty() && !is_system(a)) {
        args.push("/grant:r".into());
        args.push(format!("{}:(OI)(CI)M", account_spec(a)).into());
    }
    args
}

const USERS_SID: &str = "S-1-5-32-545";

/// The icacls arguments for the install root itself (P2.16): inheritance
/// removed, so the `Authenticated Users:Modify` that `C:\` hands down is gone;
/// SYSTEM and Administrators Full; Users Read & execute (docs, logs, CLI);
/// the service account Read & execute, so it can run the executable whether or
/// not Windows puts it in Users. Its write rights are granted per folder.
/// Pure; `/grant:r` makes a second run change nothing.
pub fn root_lockdown_args(root: &Path, service_account: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        root.as_os_str().to_os_string(),
        "/inheritance:r".into(),
        "/grant:r".into(),
        format!("*{SYSTEM_SID}:(OI)(CI)F").into(),
        "/grant:r".into(),
        format!("*{ADMINISTRATORS_SID}:(OI)(CI)F").into(),
        "/grant:r".into(),
        format!("*{USERS_SID}:(OI)(CI)RX").into(),
    ];
    if let Some(a) = service_account.filter(|a| !a.trim().is_empty() && !is_system(a)) {
        args.push("/grant:r".into());
        args.push(format!("{}:(OI)(CI)RX", account_spec(a)).into());
    }
    args
}

/// The icacls arguments that strip every explicit grant `account` holds on the
/// root and below (`/T`, errors on single files skipped with `/C`). An older
/// installer ran `/grant account:(OI)(CI)M /T` on the whole root, leaving an
/// explicit ACE on the root and on every file, executable included;
/// `/inheritance:r` and `/grant:r` do not remove those. Pure.
pub fn remove_grants_args(root: &Path, account: &str) -> Vec<OsString> {
    vec![
        root.as_os_str().to_os_string(),
        "/remove:g".into(),
        account_spec(account).into(),
        "/T".into(),
        "/C".into(),
        "/Q".into(),
    ]
}

/// [`remove_grants_args`] for `path` itself only, without `/T`: the root when
/// its tree holds a link that `/T` would follow. Pure.
pub fn remove_grants_here_args(path: &Path, account: &str) -> Vec<OsString> {
    vec![path.as_os_str().to_os_string(), "/remove:g".into(), account_spec(account).into(), "/C".into(), "/Q".into()]
}

/// Strip `account`'s explicit grants from the root tree (see [`remove_grants_args`]).
/// LocalSystem has none to strip. The caller re-grants what is needed afterwards.
pub async fn remove_grants(root: &Path, account: &str) -> Result<()> {
    remove_grants_with(root, account, true).await
}

/// [`remove_grants`] on `path` and below (`recursive`), or on `path` alone.
pub async fn remove_grants_with(root: &Path, account: &str, recursive: bool) -> Result<()> {
    if is_system(account) || account.trim().is_empty() {
        return Ok(());
    }
    let args = if recursive { remove_grants_args(root, account) } else { remove_grants_here_args(root, account) };
    let o = crate::process::run_capture(&icacls()?, args, Duration::from_secs(600)).await?;
    if o.success {
        Ok(())
    } else {
        Err(anyhow!("icacls exit {:?} removing {} from {}", o.exit_code, account, root.display()))
    }
}

/// Lock the install root (see [`root_lockdown_args`]). Errors are for the caller to report.
pub async fn lock_root(root: &Path, service_account: Option<&str>) -> Result<()> {
    let o = crate::process::run_capture(&icacls()?, root_lockdown_args(root, service_account), Duration::from_secs(120)).await?;
    if o.success {
        info!("Install folder permissions restricted: {}", root.display());
        Ok(())
    } else {
        Err(anyhow!("icacls exit {:?} on {}", o.exit_code, root.display()))
    }
}

/// The SID in `whoami /user /fo csv /nh` output: `"DOMAIN\user","S-1-5-..."`.
pub fn parse_whoami_sid(csv: &str) -> Option<String> {
    csv.lines()
        .find_map(|l| l.trim().rsplit(',').next().map(|f| f.trim().trim_matches('"').to_string()))
        .filter(|s| s.starts_with("S-1-") && s.len() < 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

pub async fn current_sid() -> Option<String> {
    let o = crate::process::run_capture(&whoami().ok()?, ["/user", "/fo", "csv", "/nh"], Duration::from_secs(20)).await.ok()?;
    o.success.then(|| parse_whoami_sid(&o.stdout)).flatten()
}

/// What to do about the folders at start-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Re-apply the restriction: only LocalSystem may (it is the owner and has WRITE_DAC).
    Apply,
    /// Only check that nobody broad has access: a named service account has Modify, which
    /// cannot change an ACL, so a re-apply would fail every time.
    Verify,
}

/// SYSTEM re-applies; any other account, or one that could not be found, verifies.
pub fn action_for(current_sid: Option<&str>) -> Action {
    if current_sid.map(str::trim) == Some(SYSTEM_SID) {
        Action::Apply
    } else {
        Action::Verify
    }
}

/// Whether `icacls` output (the readable listing, or the SDDL `/save` writes,
/// which is the same whatever the Windows language) still gives access to
/// Users, Authenticated Users or Everyone.
pub fn open_to_everyone(listing: &str) -> bool {
    let lower = listing.to_ascii_lowercase();
    if ["builtin\\users", "authenticated users", "everyone"].iter().any(|n| lower.contains(n)) {
        return true;
    }
    if [";;;bu)", ";;;au)", ";;;wd)"].iter().any(|t| lower.contains(t)) {
        return true;
    }
    listing
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|tok| matches!(tok.to_ascii_uppercase().as_str(), "S-1-5-32-545" | "S-1-5-11" | "S-1-1-0"))
}

/// A `/save` file is UTF-16 with a byte order mark; plain text otherwise.
fn decode_save_file(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Check `dirs` without changing anything. `Err` names the folders that are
/// still open to everyone (or that could not be read).
pub async fn verify(dirs: &[PathBuf]) -> Result<()> {
    let mut bad: Vec<String> = Vec::new();
    for dir in dirs.iter().filter(|d| d.is_dir()) {
        let listing = crate::process::run_capture(&icacls()?, [dir.as_os_str().to_os_string()], Duration::from_secs(60)).await;
        let mut open = match &listing {
            Ok(o) if o.success => open_to_everyone(&o.stdout),
            _ => {
                bad.push(format!("{} (could not be read)", dir.display()));
                continue;
            }
        };
        // The same ACL as SDDL, whatever language the names are in.
        let save = std::env::temp_dir().join(format!("omni-acl-{}-{}.txt", std::process::id(), bad.len()));
        let args = [dir.as_os_str().to_os_string(), "/save".into(), save.as_os_str().to_os_string(), "/Q".into()];
        // The listing names groups in the Windows language, so without the
        // SDDL nothing was really checked: say so instead of reporting green.
        let saved = match crate::process::run_capture(&icacls()?, args, Duration::from_secs(60)).await {
            Ok(o) if o.success => std::fs::read(&save).ok(),
            _ => None,
        };
        let _ = std::fs::remove_file(&save);
        match saved {
            Some(bytes) => open |= open_to_everyone(&decode_save_file(&bytes)),
            None => {
                bad.push(format!("{} (could not be read)", dir.display()));
                continue;
            }
        }
        if open {
            bad.push(format!("{} (still open to Users or Everyone)", dir.display()));
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("{}", bad.join(", ")))
    }
}

/// Lock down each of `dirs`. An error means something was not applied; the
/// caller reports it and carries on, it must not stop the service.
pub async fn apply(dirs: &[PathBuf], account: Account) -> Result<()> {
    let extra: Option<String> = match account {
        Account::System => None,
        Account::Named(n) => Some(n),
        Account::Current => match current_sid().await {
            Some(sid) => Some(sid),
            None => return Err(anyhow!("could not tell which account the service runs as; folder permissions left as they are")),
        },
    };
    let mut failed: Vec<String> = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let args = lockdown_args(dir, extra.as_deref());
        match crate::process::run_capture(&icacls()?, args, Duration::from_secs(60)).await {
            Ok(o) if o.success => info!("Folder permissions restricted: {}", dir.display()),
            Ok(o) => failed.push(format!("{} (icacls exit {:?})", dir.display(), o.exit_code)),
            Err(e) => failed.push(format!("{} ({e:#})", dir.display())),
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("could not restrict {}", failed.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installer runs these elevated: a bare name would run a planted
    /// `icacls.exe` from the folder Setup or the release sits in.
    #[cfg(windows)]
    #[test]
    fn icacls_and_whoami_are_run_from_the_system_directory_by_full_path() {
        let system = crate::paths::system_tool("kernel32.dll").unwrap();
        let system = system.parent().unwrap();
        for (tool, name) in [(icacls().unwrap(), "icacls.exe"), (whoami().unwrap(), "whoami.exe")] {
            assert!(tool.is_absolute(), "{tool:?}");
            assert_eq!(tool.parent(), Some(system), "{tool:?}");
            assert_eq!(tool.file_name().unwrap(), name);
        }
    }

    #[test]
    fn the_work_folder_and_the_install_root_have_their_own_descriptors() {
        assert_eq!(PROTECTED_SDDL, "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)");
        // P2.16 baseline: Users may read and run; the service account is added per folder later.
        assert_eq!(INSTALL_ROOT_SDDL, "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)");
    }

    /// S4: the user running the installer is not a trusted owner.
    #[test]
    fn only_administrators_system_and_trusted_installer_are_trusted_owners() {
        for sid in [ADMINISTRATORS_SID, SYSTEM_SID, TRUSTED_INSTALLER_SID, " s-1-5-18 "] {
            assert!(is_trusted_owner(sid), "{sid}");
        }
        for sid in ["S-1-5-21-1-2-3-1001", "S-1-5-11", "S-1-5-32-545", "S-1-1-0", "S-1-3-0", ""] {
            assert!(!is_trusted_owner(sid), "{sid}");
        }
    }

    /// The real reader, judged by the pure rule whatever the test run's elevation.
    #[test]
    fn a_folder_s_owner_is_judged_by_the_trusted_owner_rule_and_a_missing_one_not_at_all() {
        let d = tempfile::tempdir().unwrap();
        let sid = owner_sid(d.path()).expect("the owner of a folder this test made");
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(foreign_owner(d.path()).is_some(), !is_trusted_owner(&sid));
        assert_eq!(foreign_owner(&d.path().join("missing")), None);
        assert!(!read_dacl(d.path()).unwrap().is_empty());
    }

    fn ace(sid: &str, mask: u32, flags: u8, allow: bool) -> Ace {
        Ace { sid: sid.into(), mask, flags, allow, name: format!("name-of-{sid}") }
    }

    /// S2c: who could swap the folder an install root sits in.
    #[test]
    fn a_folder_anyone_else_may_rename_or_re_permission_is_found() {
        const MODIFY: u32 = 0x0013_01bf;
        const READ_EXECUTE: u32 = 0x0012_00a9;
        const FULL: u32 = 0x001f_01ff;
        let admins = ace(ADMINISTRATORS_SID, FULL, 0x03, true);
        let system = ace(SYSTEM_SID, FULL, 0x03, true);
        let fine = [admins.clone(), system.clone(), ace("S-1-5-32-545", READ_EXECUTE, 0x03, true)];
        assert_eq!(swap_principal(&fine), None, "Users read & execute only");
        let au = [admins.clone(), ace("S-1-5-11", MODIFY, 0x13, true)];
        assert_eq!(swap_principal(&au), Some("name-of-S-1-5-11".into()), "Authenticated Users Modify (inherited)");
        let inherit_only = [admins.clone(), ace("S-1-5-11", MODIFY, 0x0b, true), ace("S-1-3-0", 0x1000_0000, 0x0b, true)];
        assert_eq!(swap_principal(&inherit_only), None, "only for what is created inside");
        let deny = [admins.clone(), ace("S-1-5-11", MODIFY, 0x00, false)];
        assert_eq!(swap_principal(&deny), None, "a deny entry gives nothing");
        assert_eq!(swap_principal(&[admins, system]), None, "Administrators and SYSTEM Full");
        // Each swap right alone, and GENERIC_ALL.
        for right in [0x0001_0000, 0x0004_0000, 0x0008_0000, 0x0000_0040, 0x1000_0000] {
            assert!(swap_principal(&[ace("S-1-5-21-9-9-9-1001", right, 0, true)]).is_some(), "{right:#x}");
        }
        assert_eq!(swap_principal(&[ace("S-1-5-21-9-9-9-1001", 0x0000_0006, 0, true)]), None, "create files and folders only");
    }

    /// T6: an owner always has WRITE_DAC, so an untrusted owner swaps like a right.
    #[test]
    fn an_untrusted_owner_makes_a_folder_swappable_whatever_its_entries() {
        const FULL: u32 = 0x001f_01ff;
        let clean = [ace(ADMINISTRATORS_SID, FULL, 0x03, true), ace("S-1-5-32-545", 0x0012_00a9, 0x03, true)];
        assert_eq!(swap_reason(Some((ADMINISTRATORS_SID, "BUILTIN\\Administrators")), &clean), None);
        assert_eq!(swap_reason(Some((TRUSTED_INSTALLER_SID, "NT SERVICE\\TrustedInstaller")), &clean), None);
        assert_eq!(
            swap_reason(Some(("S-1-5-21-9-9-9-1001", r"NEWSROOM\someone")), &clean),
            Some(r"NEWSROOM\someone (its owner)".into())
        );
        assert!(swap_reason(None, &clean).is_some(), "an owner that cannot be read");
        // A trusted owner with a swap right for someone else: the entry decides.
        let au = [ace("S-1-5-11", 0x0013_01bf, 0x13, true)];
        assert_eq!(swap_reason(Some((SYSTEM_SID, "SYSTEM")), &au), Some("name-of-S-1-5-11".into()));
    }

    /// U3: an existing folder that is not an installation must not let anyone
    /// else put files in it (inherit-only entries count: they reach bin\ and data\).
    #[test]
    fn a_folder_anyone_else_may_write_into_is_found_inherit_only_entries_included() {
        const FULL: u32 = 0x001f_01ff;
        const MODIFY: u32 = 0x0013_01bf;
        const RX: u32 = 0x0012_00a9;
        let base = [
            ace(ADMINISTRATORS_SID, FULL, 0x03, true),
            ace(SYSTEM_SID, FULL, 0x03, true),
            ace("S-1-5-32-545", RX, 0x03, true),
        ];
        assert_eq!(write_principal(&base), None, "INSTALL_ROOT_SDDL's entries");
        let mut p216 = base.to_vec();
        p216.push(ace("S-1-5-80-1-2-3-4-5", RX, 0x03, true));
        assert_eq!(write_principal(&p216), None, "the P2.16 root: service account read & execute");
        let mut inherited = base.to_vec();
        inherited.push(ace("S-1-5-11", MODIFY, 0x13, true));
        assert_eq!(write_principal(&inherited), Some("name-of-S-1-5-11".into()), "inherited Authenticated Users Modify");
        let mut inherit_only = base.to_vec();
        inherit_only.push(ace("S-1-5-11", MODIFY, 0x0b, true));
        assert_eq!(write_principal(&inherit_only), Some("name-of-S-1-5-11".into()), "inherit-only counts here");
        let mut creator = base.to_vec();
        creator.push(ace(CREATOR_OWNER_SID, 0x1000_0000, 0x0b, true));
        assert_eq!(write_principal(&creator), None, "CREATOR OWNER");
        let mut deny = base.to_vec();
        deny.push(ace("S-1-5-11", MODIFY, 0x00, false));
        assert_eq!(write_principal(&deny), None);
        for right in [0x2, 0x4, 0x40, 0x0001_0000, 0x0004_0000, 0x0008_0000, 0x4000_0000, 0x1000_0000] {
            assert!(write_principal(&[ace("S-1-5-21-9-9-9-1001", right, 0, true)]).is_some(), "{right:#x}");
        }
    }

    /// T8: a conditional allow entry may hold, so it is read as an allow.
    #[test]
    fn callback_allow_entries_are_allows_and_object_entries_are_skipped() {
        assert_eq!(ace_kind(0), Some(true));
        assert_eq!(ace_kind(9), Some(true), "ACCESS_ALLOWED_CALLBACK_ACE");
        assert_eq!(ace_kind(1), Some(false));
        assert_eq!(ace_kind(10), Some(false), "ACCESS_DENIED_CALLBACK_ACE");
        for skipped in [2u8, 5, 6, 7, 11, 12] {
            assert_eq!(ace_kind(skipped), None, "{skipped}");
        }
    }

    fn strings(a: Vec<OsString>) -> Vec<String> {
        a.into_iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_arguments_remove_inheritance_and_grant_only_system_admins_and_the_service() {
        let a = strings(lockdown_args(Path::new("C:/Omni/data"), Some("NT SERVICE\\OmniIngestService")));
        assert_eq!(a[0], "C:/Omni/data");
        assert_eq!(a[1], "/inheritance:r");
        let grants: Vec<&String> = a.iter().skip(2).filter(|x| *x != "/grant:r").collect();
        assert_eq!(
            grants,
            [&"*S-1-5-18:(OI)(CI)F".to_string(), &"*S-1-5-32-544:(OI)(CI)F".to_string(), &"NT SERVICE\\OmniIngestService:(OI)(CI)M".to_string()]
        );
        // Every grant is a replace, so a second run changes nothing.
        assert_eq!(a.iter().filter(|x| *x == "/grant:r").count(), 3);
        assert!(!a.iter().any(|x| x == "/grant" || x.contains("Everyone") || x.contains("Users")));
    }

    #[test]
    fn the_root_keeps_users_read_only_and_drops_the_inherited_modify() {
        let a = strings(root_lockdown_args(Path::new("C:/Omni"), Some("NT SERVICE\\OmniIngestService")));
        assert_eq!(a[0], "C:/Omni");
        assert_eq!(a[1], "/inheritance:r");
        let grants: Vec<&String> = a.iter().skip(2).filter(|x| *x != "/grant:r").collect();
        assert_eq!(
            grants,
            [
                &"*S-1-5-18:(OI)(CI)F".to_string(),
                &"*S-1-5-32-544:(OI)(CI)F".to_string(),
                &"*S-1-5-32-545:(OI)(CI)RX".to_string(),
                &"NT SERVICE\\OmniIngestService:(OI)(CI)RX".to_string(),
            ]
        );
        let all = a.join(" ");
        assert!(!all.contains("S-1-5-11") && !all.contains("S-1-1-0"), "no Authenticated Users, no Everyone");
        // Nothing but Users is read-only-and-broad: no write right for anyone but SYSTEM/Admins.
        assert!(grants.iter().filter(|g| !g.contains("S-1-5-18") && !g.contains("S-1-5-32-544")).all(|g| g.ends_with(")RX")));
        // LocalSystem: no extra entry.
        assert_eq!(root_lockdown_args(Path::new("d"), None).len(), 8);
        assert_eq!(root_lockdown_args(Path::new("d"), Some("LocalSystem")).len(), 8);
    }

    #[test]
    fn a_users_read_entry_is_what_the_open_check_flags_on_data_and_not_on_the_root_listing_we_write() {
        // The root result as icacls would list it: Users read-only is intended there...
        let root = "C:\\Omni BUILTIN\\Users:(OI)(CI)(RX)\n NT AUTHORITY\\SYSTEM:(OI)(CI)(F)\n BUILTIN\\Administrators:(OI)(CI)(F)";
        // ...and exactly what the data\ check must keep refusing (negative control).
        assert!(open_to_everyone(root), "the data/temp check still flags a Users entry");
        // What data\ looks like after the lockdown (no inheritance from the root) is clean.
        let data = "C:\\Omni\\data NT AUTHORITY\\SYSTEM:(OI)(CI)(F)\n BUILTIN\\Administrators:(OI)(CI)(F)\n NT SERVICE\\OmniIngestService:(OI)(CI)(M)";
        assert!(!open_to_everyone(data));
        // The args we emit for the root carry no Authenticated Users or Everyone.
        let args = strings(root_lockdown_args(Path::new("r"), None)).join(" ");
        assert!(!open_to_everyone(&args.replace("*S-1-5-32-545", "")), "Users RX is the only broad entry");
    }

    #[test]
    fn the_old_whole_root_grant_is_removed_recursively_before_the_lockdown() {
        let a = strings(remove_grants_args(Path::new("C:/Omni"), "NT SERVICE\\OmniIngestService"));
        assert_eq!(a, ["C:/Omni", "/remove:g", "NT SERVICE\\OmniIngestService", "/T", "/C", "/Q"]);
        assert_eq!(strings(remove_grants_args(Path::new("r"), "S-1-5-80-1"))[2], "*S-1-5-80-1");
    }

    #[test]
    fn localsystem_adds_nothing_and_a_sid_is_starred() {
        for system in [None, Some("LocalSystem"), Some("S-1-5-18"), Some("NT AUTHORITY\\SYSTEM"), Some("  ")] {
            assert_eq!(lockdown_args(Path::new("d"), system).len(), 6, "{system:?}");
        }
        let a = strings(lockdown_args(Path::new("d"), Some("S-1-5-80-1-2-3")));
        assert_eq!(a.last().unwrap(), "*S-1-5-80-1-2-3:(OI)(CI)M");
    }

    #[test]
    fn the_whoami_sid_is_read_and_junk_is_not() {
        assert_eq!(parse_whoami_sid("\"NT AUTHORITY\\SYSTEM\",\"S-1-5-18\"\r\n").as_deref(), Some("S-1-5-18"));
        assert_eq!(parse_whoami_sid("\"PC\\svc-omni\",\"S-1-5-21-1-2-3-1001\"").as_deref(), Some("S-1-5-21-1-2-3-1001"));
        assert_eq!(parse_whoami_sid("ERROR: nope"), None);
        assert_eq!(parse_whoami_sid(""), None);
    }
    #[test]
    fn system_reapplies_and_every_other_account_only_verifies() {
        assert_eq!(action_for(Some("S-1-5-18")), Action::Apply);
        assert_eq!(action_for(Some("S-1-5-80-1-2-3")), Action::Verify);
        assert_eq!(action_for(Some("S-1-5-21-1-2-3-1001")), Action::Verify);
        assert_eq!(action_for(None), Action::Verify, "an unknown account must never try to change the ACL");
    }

    #[test]
    fn a_restricted_listing_is_fine_and_a_broad_group_is_not() {
        let restricted = "C:\\Omni\\data NT AUTHORITY\\SYSTEM:(OI)(CI)(F)
       BUILTIN\\Administrators:(OI)(CI)(F)
       NT SERVICE\\OmniIngestService:(OI)(CI)(M)

Successfully processed 1 files; Failed processing 0 files";
        assert!(!open_to_everyone(restricted));
        for broad in [
            "C:\\Omni\\data BUILTIN\\Users:(OI)(CI)(RX)
 NT AUTHORITY\\SYSTEM:(F)",
            "x NT AUTHORITY\\Authenticated Users:(OI)(CI)(M)",
            "x Everyone:(F)",
            "x *S-1-5-32-545:(OI)(CI)(RX)",
            "x S-1-5-11:(M)",
            "x S-1-1-0:(F)",
        ] {
            assert!(open_to_everyone(broad), "{broad}");
        }
        // A longer SID that merely starts the same is another account.
        assert!(!open_to_everyone("x S-1-5-110:(F) S-1-5-32-5450:(F)"));
    }

    #[test]
    fn the_sddl_is_read_the_same_in_any_windows_language() {
        // Names are localized in the listing; the SDDL trustees are not.
        let restricted = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;S-1-5-80-1-2-3)";
        assert!(!open_to_everyone(restricted));
        assert!(open_to_everyone("D:PAI(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)"));
        assert!(open_to_everyone("D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;WD)"));
        assert!(open_to_everyone("D:PAI(A;OICI;FA;;;AU)"));
        // A UTF-16 /save file decodes.
        let mut bytes = vec![0xFF, 0xFE];
        for u in "D:PAI(A;;FA;;;BU)".encode_utf16() {
            bytes.extend(u.to_le_bytes());
        }
        assert!(open_to_everyone(&decode_save_file(&bytes)));
    }
}
