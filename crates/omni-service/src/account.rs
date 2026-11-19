//! A named service account's two preconditions, checked before the service is
//! touched (P13.1b2): the right to log on as a service, and a password Windows
//! accepts for a service logon. The password is never logged, put on a command
//! line or into an error text.

use anyhow::Result;

/// What a service logon with the given password ended with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogonCheck {
    Ok,
    WrongPassword,
    /// The account may not log on as a service (a policy denies it).
    NotAllowed,
    /// Expired, or must be changed at the next logon.
    PasswordExpired,
    Disabled,
    LockedOut,
    /// No domain controller could be reached to check the account.
    NoLogonServer,
    /// Any other Windows error code.
    Other(u32),
}

impl LogonCheck {
    /// One sentence for the operator saying what to fix; `None` for `Ok`.
    /// Never holds the password (it is not given one).
    pub fn operator_text(&self, account: &str) -> Option<String> {
        Some(match self {
            LogonCheck::Ok => return None,
            LogonCheck::WrongPassword => {
                format!("Windows did not accept the password given for {account}; check it and run Setup again.")
            }
            LogonCheck::NotAllowed => format!(
                "{account} may not log on as a service on this computer (a Group Policy denies it); ask the domain \
                 administrator to allow \"Log on as a service\" for it, then run Setup again."
            ),
            LogonCheck::PasswordExpired => {
                format!("The password of {account} has expired or must be changed; change it, then run Setup again with the new one.")
            }
            LogonCheck::Disabled => format!("The account {account} is disabled; enable it or choose another account."),
            LogonCheck::LockedOut => format!("The account {account} is locked out; unlock it, then run Setup again."),
            LogonCheck::NoLogonServer => format!(
                "Windows could not reach a domain controller to check {account}. Check the network and try again; \
                 nothing is wrong with the password as far as Setup can tell."
            ),
            LogonCheck::Other(code) => format!(
                "Windows could not log on as {account} (error {code}); check the account and its password, then run Setup again."
            ),
        })
    }
}

/// The Win32 error of a failed `LogonUserW`, read. Pure.
pub fn logon_check_from_error(code: u32) -> LogonCheck {
    match code {
        0 => LogonCheck::Ok,
        1326 => LogonCheck::WrongPassword,
        1385 => LogonCheck::NotAllowed,
        1330 | 1907 => LogonCheck::PasswordExpired,
        1331 => LogonCheck::Disabled,
        1909 => LogonCheck::LockedOut,
        // ERROR_NO_LOGON_SERVERS, ERROR_TRUSTED_RELATIONSHIP_FAILURE.
        1311 | 1789 => LogonCheck::NoLogonServer,
        other => LogonCheck::Other(other),
    }
}

/// `DOMAIN\user` → (`DOMAIN`, `user`); `.\user` → (`.`, `user`), the local
/// account database; `user@domain` → (none, `user@domain`), which `LogonUserW`
/// takes only without a domain; a bare `user` → (none, `user`). Pure.
pub fn split_account(account: &str) -> (Option<&str>, &str) {
    let a = account.trim();
    match a.split_once('\\') {
        Some((domain, user)) => (Some(domain), user),
        None => (None, a),
    }
}

/// Give `account` the right to log on as a service (`SeServiceLogonRight`).
/// Granting a right the account already has changes nothing.
#[cfg(windows)]
pub fn grant_service_logon_right(account: &str) -> Result<()> {
    sys::grant_service_logon_right(account)
}

/// Whether Windows accepts `password` for a service logon of `account`; the
/// token is closed at once.
#[cfg(windows)]
pub fn check_service_logon(account: &str, password: &str) -> LogonCheck {
    sys::check_service_logon(account, password)
}

#[cfg(not(windows))]
pub fn grant_service_logon_right(_account: &str) -> Result<()> {
    anyhow::bail!("Service logon rights exist only on Windows.");
}

#[cfg(not(windows))]
pub fn check_service_logon(_account: &str, _password: &str) -> LogonCheck {
    LogonCheck::Other(0)
}

#[cfg(windows)]
mod sys {
    use super::{logon_check_from_error, split_account, LogonCheck};
    use anyhow::{bail, Result};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::Authentication::Identity::{
        LsaAddAccountRights, LsaClose, LsaNtStatusToWinError, LsaOpenPolicy, LSA_HANDLE, LSA_OBJECT_ATTRIBUTES,
        LSA_UNICODE_STRING, POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
    };
    use windows::Win32::Security::{
        LogonUserW, LookupAccountNameW, LOGON32_LOGON_SERVICE, LOGON32_PROVIDER_DEFAULT, PSID, SID_NAME_USE,
    };

    /// The name `LookupAccountNameW` takes: it does not know `.\` for this computer.
    pub(super) fn lookup_name(account: &str) -> &str {
        let a = account.trim();
        a.strip_prefix(r".\").unwrap_or(a)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The Win32 code in a windows-rs error built from one.
    fn win32_code(e: &windows::core::Error) -> u32 {
        e.code().0 as u32 & 0xFFFF
    }

    /// The SID of `account`, as the bytes `LookupAccountNameW` wrote.
    fn sid_of(account: &str) -> Result<Vec<u8>> {
        let name = wide(lookup_name(account));
        let (mut sid_len, mut domain_len) = (0u32, 0u32);
        let mut kind = SID_NAME_USE::default();
        // SAFETY: a size query: nothing is written with zero lengths.
        let _ = unsafe {
            LookupAccountNameW(PCWSTR::null(), PCWSTR(name.as_ptr()), PSID::default(), &mut sid_len, PWSTR::null(), &mut domain_len, &mut kind)
        };
        if sid_len == 0 {
            bail!("Windows does not know the account {account}");
        }
        let mut sid = vec![0u8; sid_len as usize];
        let mut domain = vec![0u16; domain_len.max(1) as usize];
        // SAFETY: both buffers have the lengths passed with them and outlive the call.
        unsafe {
            LookupAccountNameW(
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                PSID(sid.as_mut_ptr().cast()),
                &mut sid_len,
                PWSTR(domain.as_mut_ptr()),
                &mut domain_len,
                &mut kind,
            )
        }
        .map_err(|e| anyhow::anyhow!("Windows could not look up the account {account} (error {})", win32_code(&e)))?;
        Ok(sid)
    }

    pub fn grant_service_logon_right(account: &str) -> Result<()> {
        let mut sid = sid_of(account)?;
        let attributes = LSA_OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<LSA_OBJECT_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        let mut policy = LSA_HANDLE::default();
        // SAFETY: `attributes` and `policy` outlive the call.
        let status =
            unsafe { LsaOpenPolicy(None, &attributes, (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32, &mut policy) };
        if status.is_err() {
            // SAFETY: a plain conversion of a status code.
            let code = unsafe { LsaNtStatusToWinError(status) };
            bail!("Windows refused to open its local security policy (error {code})");
        }
        let mut right: Vec<u16> = "SeServiceLogonRight".encode_utf16().collect();
        let bytes = (right.len() * 2) as u16;
        let rights = [LSA_UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: PWSTR(right.as_mut_ptr()) }];
        // SAFETY: the SID and the right's buffer outlive the call; the policy is closed once.
        let status = unsafe { LsaAddAccountRights(policy, PSID(sid.as_mut_ptr().cast()), &rights) };
        // SAFETY: `policy` was opened above and is closed once.
        let _ = unsafe { LsaClose(policy) };
        if status.is_err() {
            // SAFETY: a plain conversion of a status code.
            let code = unsafe { LsaNtStatusToWinError(status) };
            bail!("Windows refused to give {account} the right to log on as a service (error {code})");
        }
        Ok(())
    }

    pub fn check_service_logon(account: &str, password: &str) -> LogonCheck {
        let (domain, user) = split_account(account);
        let user = wide(user);
        let domain = domain.map(wide);
        let mut secret = wide(password);
        let mut token = HANDLE::default();
        // SAFETY: every string is NUL-terminated and outlives the call; the token is closed once.
        let result = unsafe {
            LogonUserW(
                PCWSTR(user.as_ptr()),
                domain.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
                PCWSTR(secret.as_ptr()),
                LOGON32_LOGON_SERVICE,
                LOGON32_PROVIDER_DEFAULT,
                &mut token,
            )
        };
        // The password's copy is not left in memory longer than needed.
        secret.iter_mut().for_each(|c| *c = 0);
        match result {
            Ok(()) => {
                // SAFETY: the token was returned by a successful logon and is closed once.
                let _ = unsafe { CloseHandle(token) };
                LogonCheck::Ok
            }
            Err(e) => match logon_check_from_error(win32_code(&e)) {
                // A failed call that reports no error is still a failure.
                LogonCheck::Ok => LogonCheck::Other(0),
                other => other,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_is_split_the_way_logon_user_takes_it() {
        assert_eq!(split_account(r"NEWSROOM\svc_test"), (Some("NEWSROOM"), "svc_test"));
        assert_eq!(split_account(r".\omni_test"), (Some("."), "omni_test"));
        assert_eq!(split_account("svc_test@newsroom.example"), (None, "svc_test@newsroom.example"));
        assert_eq!(split_account("svc_test"), (None, "svc_test"));
        assert_eq!(split_account(r"  NEWSROOM\svc_test "), (Some("NEWSROOM"), "svc_test"));
    }

    #[cfg(windows)]
    #[test]
    fn the_lookup_name_drops_the_local_prefix_only() {
        assert_eq!(sys::lookup_name(r".\omni_test"), "omni_test");
        assert_eq!(sys::lookup_name(r"NEWSROOM\svc_test"), r"NEWSROOM\svc_test");
    }

    #[test]
    fn each_logon_error_is_read_as_what_the_operator_must_fix() {
        assert_eq!(logon_check_from_error(1326), LogonCheck::WrongPassword);
        assert_eq!(logon_check_from_error(1385), LogonCheck::NotAllowed);
        assert_eq!(logon_check_from_error(1330), LogonCheck::PasswordExpired);
        assert_eq!(logon_check_from_error(1907), LogonCheck::PasswordExpired);
        assert_eq!(logon_check_from_error(1331), LogonCheck::Disabled);
        assert_eq!(logon_check_from_error(1909), LogonCheck::LockedOut);
        assert_eq!(logon_check_from_error(1311), LogonCheck::NoLogonServer);
        assert_eq!(logon_check_from_error(1789), LogonCheck::NoLogonServer);
        assert_eq!(logon_check_from_error(5), LogonCheck::Other(5));
        assert_eq!(logon_check_from_error(0), LogonCheck::Ok);
    }

    #[test]
    fn every_failed_check_has_one_sentence_and_ok_has_none() {
        let account = r"NEWSROOM\svc_test";
        assert_eq!(LogonCheck::Ok.operator_text(account), None);
        for c in [
            LogonCheck::WrongPassword,
            LogonCheck::NotAllowed,
            LogonCheck::PasswordExpired,
            LogonCheck::Disabled,
            LogonCheck::LockedOut,
            LogonCheck::Other(87),
        ] {
            let text = c.operator_text(account).unwrap();
            assert!(text.contains(account), "{c:?}: {text}");
            assert!(text.ends_with('.'), "{c:?}: {text}");
            assert_eq!(text.matches(". ").count(), 0, "one sentence: {text}");
        }
        assert!(LogonCheck::Other(87).operator_text(account).unwrap().contains("error 87"));
        // V7: not a password problem; the text the lead gave, two sentences.
        assert_eq!(
            LogonCheck::NoLogonServer.operator_text(account).unwrap(),
            r"Windows could not reach a domain controller to check NEWSROOM\svc_test. Check the network and try again; nothing is wrong with the password as far as Setup can tell."
        );
        assert!(LogonCheck::NotAllowed.operator_text(account).unwrap().contains("Log on as a service"));
    }
}
