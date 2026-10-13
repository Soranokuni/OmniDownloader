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

/// The SID in `whoami /user /fo csv /nh` output: `"DOMAIN\user","S-1-5-..."`.
pub fn parse_whoami_sid(csv: &str) -> Option<String> {
    csv.lines()
        .find_map(|l| l.trim().rsplit(',').next().map(|f| f.trim().trim_matches('"').to_string()))
        .filter(|s| s.starts_with("S-1-") && s.len() < 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

pub async fn current_sid() -> Option<String> {
    let o = crate::process::run_capture(Path::new("whoami"), ["/user", "/fo", "csv", "/nh"], Duration::from_secs(20)).await.ok()?;
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
        let listing = crate::process::run_capture(Path::new("icacls"), [dir.as_os_str().to_os_string()], Duration::from_secs(60)).await;
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
        let saved = match crate::process::run_capture(Path::new("icacls"), args, Duration::from_secs(60)).await {
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
        match crate::process::run_capture(Path::new("icacls"), args, Duration::from_secs(60)).await {
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
