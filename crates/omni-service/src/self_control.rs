//! The service may stop and start itself (P13.2).
//!
//! The admin panel's «Επανεκκίνηση υπηρεσίας» stops the service through the
//! SCM (the graceful stop of P2.15) and a detached helper,
//! `omni-ingest service restart-self`, starts it again once it has stopped.
//! Both run as the service's own account, which by default has no right over
//! its own service. The installer grants it exactly start, stop and query
//! status on this one service, by adding one access-control entry to the
//! service's DACL: [`dacl_granting_self_control`] builds the new DACL from the
//! old one (pure, tested); [`grant_self_control`] reads, rewrites and writes it
//! (Windows only, at install, upgrade and account change; never in tests).
//! For NetworkService and LocalService the entry names the service's own SID,
//! see [`grantee`] (P13.2b).

use anyhow::{Context, Result};

/// SERVICE_START (`RP`, 0x10), SERVICE_STOP (`WP`, 0x20) and
/// SERVICE_QUERY_STATUS (`LC`, 0x4), in SDDL. Nothing else: not change config,
/// not delete, not write the DACL.
pub const SELF_CONTROL_RIGHTS: &str = "RPWPLC";

/// The entry this module adds, for `sid`.
pub fn self_control_ace(sid: &str) -> String {
    format!("(A;;{SELF_CONTROL_RIGHTS};;;{sid})")
}

/// `dacl` (an SDDL string holding a `D:` part, as Windows gives a service's
/// DACL) with the self-control entry for `sid`, and only for it: an entry this
/// module added for another account (an earlier service account) is removed,
/// and the rest is kept in its order. Running it twice gives the same string.
pub fn dacl_granting_self_control(dacl: &str, sid: &str) -> Result<String> {
    let sid = sid.trim();
    if !sid.starts_with("S-1-") || !sid[4..].chars().all(|c| c.is_ascii_digit() || c == '-') {
        anyhow::bail!("not a SID: {sid:?}");
    }
    let start = dacl.find("D:").context("the security descriptor has no DACL (D:)")? + 2;
    let end = dacl[start..].find("S:").map_or(dacl.len(), |i| start + i);
    let body = &dacl[start..end];
    let first = body.find('(').unwrap_or(body.len());
    let (flags, mut rest) = body.split_at(first);
    let mut aces = Vec::new();
    while !rest.is_empty() {
        if !rest.starts_with('(') {
            anyhow::bail!("unexpected text in the DACL: {rest:?}");
        }
        let close = rest.find(')').context("an entry of the DACL is not closed")?;
        aces.push(&rest[..=close]);
        rest = &rest[close + 1..];
    }
    let ours = format!("(A;;{SELF_CONTROL_RIGHTS};;;");
    let mut out: Vec<String> = aces.into_iter().filter(|a| !a.starts_with(&ours)).map(String::from).collect();
    out.push(self_control_ace(sid));
    Ok(format!("{}{}{}{}", &dacl[..start], flags, out.concat(), &dacl[end..]))
}

/// Whether `account` needs the grant: LocalSystem already controls every service.
pub fn needs_self_control(account: Option<&str>) -> bool {
    match account.map(|a| a.trim().to_ascii_uppercase()) {
        None => false,
        Some(a) => !(a.is_empty() || a == "LOCALSYSTEM" || a == "NT AUTHORITY\\SYSTEM" || a == "NT AUTHORITY\\LOCALSYSTEM"),
    }
}

/// Who the entry is for (P13.2b). NetworkService and LocalService are shared
/// by many services: a grant to S-1-5-20 or S-1-5-19 would let every one of
/// them stop this service. For those the entry names this service's own SID,
/// `NT SERVICE\<service>`, which only this service's processes carry (the
/// installer makes the SID part of the service's token). Any other account
/// (the virtual account, a domain account) is its own.
pub fn grantee(service: &str, account: &str) -> String {
    let a = account.trim().to_ascii_uppercase().replace(' ', "");
    let a = a.strip_prefix("NTAUTHORITY\\").unwrap_or(&a);
    if matches!(a, "NETWORKSERVICE" | "LOCALSERVICE" | "S-1-5-20" | "S-1-5-19") {
        format!("NT SERVICE\\{service}")
    } else {
        account.trim().to_string()
    }
}

#[cfg(windows)]
mod os {
    use super::*;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        LookupAccountNameW, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
    };
    use windows::Win32::System::Services::{
        ChangeServiceConfig2W, CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceObjectSecurity,
        SetServiceObjectSecurity, SC_MANAGER_CONNECT, SERVICE_CONFIG_SERVICE_SID_INFO, SERVICE_SID_INFO,
    };

    const SERVICE_CHANGE_CONFIG: u32 = 0x0002;
    /// SERVICE_SID_TYPE_UNRESTRICTED.
    const SERVICE_SID_TYPE_UNRESTRICTED: u32 = 1;
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn pwstr_to_string(p: PWSTR) -> String {
        // SAFETY: a NUL-terminated string Windows allocated.
        unsafe { p.to_string().unwrap_or_default() }
    }

    /// The SID of `account` as a string (`S-1-5-80-…` for the virtual account).
    pub fn account_sid(account: &str) -> Result<String> {
        let name = wide(account);
        let (mut sid_len, mut dom_len) = (0u32, 0u32);
        let mut use_ = SID_NAME_USE::default();
        // SAFETY: the first call only asks for the sizes.
        unsafe {
            let _ = LookupAccountNameW(PCWSTR::null(), PCWSTR(name.as_ptr()), PSID::default(), &mut sid_len, PWSTR::null(), &mut dom_len, &mut use_);
        }
        if sid_len == 0 {
            anyhow::bail!("Windows does not know the account {account}");
        }
        let mut sid = vec![0u8; sid_len as usize];
        let mut dom = vec![0u16; dom_len.max(1) as usize];
        // SAFETY: buffers of the sizes Windows asked for.
        unsafe {
            LookupAccountNameW(
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                PSID(sid.as_mut_ptr().cast()),
                &mut sid_len,
                PWSTR(dom.as_mut_ptr()),
                &mut dom_len,
                &mut use_,
            )
            .with_context(|| format!("Looking up the account {account}"))?;
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(PSID(sid.as_mut_ptr().cast()), &mut text).context("Converting the SID")?;
            let s = pwstr_to_string(text);
            let _ = LocalFree(HLOCAL(text.0.cast()));
            Ok(s)
        }
    }

    pub fn grant(service: &str, account: &str) -> Result<()> {
        let grantee = super::grantee(service, account);
        let service_sid = grantee != account.trim();
        let name = wide(service);
        // SAFETY: handles are closed on every path; buffers outlive the calls.
        unsafe {
            let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT).context("Opening the Service Manager")?;
            let svc = match OpenServiceW(scm, PCWSTR(name.as_ptr()), READ_CONTROL | WRITE_DAC | SERVICE_CHANGE_CONFIG) {
                Ok(s) => s,
                Err(e) => {
                    let _ = CloseServiceHandle(scm);
                    return Err(e).context("Opening the service to change who may start and stop it");
                }
            };
            let result = (|| -> Result<()> {
                if service_sid {
                    // The service's SID is in its processes' token only with
                    // this SID type; it applies from the next start.
                    let info = SERVICE_SID_INFO { dwServiceSidType: SERVICE_SID_TYPE_UNRESTRICTED };
                    ChangeServiceConfig2W(svc, SERVICE_CONFIG_SERVICE_SID_INFO, Some(std::ptr::from_ref(&info).cast()))
                        .context("Adding the service's own SID to its processes")?;
                }
                let sid = account_sid(&grantee)?;
                let mut needed = 0u32;
                let _ = QueryServiceObjectSecurity(svc, DACL_SECURITY_INFORMATION.0, PSECURITY_DESCRIPTOR::default(), 0, &mut needed);
                let mut buf = vec![0u8; needed.max(1) as usize];
                QueryServiceObjectSecurity(svc, DACL_SECURITY_INFORMATION.0, PSECURITY_DESCRIPTOR(buf.as_mut_ptr().cast()), needed, &mut needed)
                    .context("Reading the service's permissions")?;
                let mut text = PWSTR::null();
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    PSECURITY_DESCRIPTOR(buf.as_mut_ptr().cast()),
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    None,
                )
                .context("Reading the service's permissions as text")?;
                let old = pwstr_to_string(text);
                let _ = LocalFree(HLOCAL(text.0.cast()));
                let new = wide(&dacl_granting_self_control(&old, &sid)?);
                let mut sd = PSECURITY_DESCRIPTOR::default();
                ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(new.as_ptr()), SDDL_REVISION_1, &mut sd, None)
                    .context("Building the service's new permissions")?;
                let set = SetServiceObjectSecurity(svc, DACL_SECURITY_INFORMATION, sd).context("Writing the service's permissions");
                let _ = LocalFree(HLOCAL(sd.0));
                set
            })();
            let _ = CloseServiceHandle(svc);
            let _ = CloseServiceHandle(scm);
            result
        }
    }
}

/// Let `account` start, stop and query the service `service` (P13.2). Called by
/// the installer after the service is created or changed, with its account
/// (`None` = LocalSystem, which needs nothing). Never fails the install: the
/// caller logs a refusal, and the panel's restart then says it cannot.
pub fn grant_self_control(service: &str, account: Option<&str>) -> Result<()> {
    let Some(account) = account.filter(|_| needs_self_control(account)) else {
        return Ok(());
    };
    #[cfg(windows)]
    return os::grant(service, account);
    #[cfg(not(windows))]
    {
        let _ = (service, account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID_A: &str = "S-1-5-80-1111111111-2222222222-3333333333-4444444444-555555555";
    const SID_B: &str = "S-1-5-21-100-200-300-1001";
    const DEFAULT: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";

    #[test]
    fn the_grant_adds_only_start_stop_and_query_for_the_account_and_keeps_the_rest() {
        let out = dacl_granting_self_control(DEFAULT, SID_A).unwrap();
        assert_eq!(out, format!("{DEFAULT}(A;;RPWPLC;;;{SID_A})"));
        assert_eq!(SELF_CONTROL_RIGHTS, "RPWPLC", "start, stop, query status: nothing more");
        // Twice is once.
        assert_eq!(dacl_granting_self_control(&out, SID_A).unwrap(), out);
    }

    #[test]
    fn a_new_service_account_replaces_the_previous_ones_grant() {
        let first = dacl_granting_self_control(DEFAULT, SID_A).unwrap();
        let second = dacl_granting_self_control(&first, SID_B).unwrap();
        assert!(!second.contains(SID_A), "{second}");
        assert!(second.ends_with(&format!("(A;;RPWPLC;;;{SID_B})")));
        assert!(second.starts_with(DEFAULT));
    }

    #[test]
    fn flags_and_a_sacl_part_survive_and_bad_input_is_refused() {
        let with = "O:SYD:P(A;;CCLCSWRPWPDTLOCRRC;;;SY)S:(AU;FA;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;WD)";
        let out = dacl_granting_self_control(with, SID_B).unwrap();
        assert_eq!(out, format!("O:SYD:P(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;RPWPLC;;;{SID_B})S:(AU;FA;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;WD)"));
        assert!(dacl_granting_self_control("O:SY", SID_A).is_err(), "no DACL");
        assert!(dacl_granting_self_control(DEFAULT, "BA").is_err(), "an alias is not a SID");
        assert!(dacl_granting_self_control(DEFAULT, "S-1-5-21)(A;;GA;;;WD").is_err(), "nothing smuggled in");
        assert!(dacl_granting_self_control("D:(A;;CC;;;SY", SID_A).is_err());
    }

    /// P13.2b: never the SID every NetworkService or LocalService process shares.
    #[test]
    fn a_shared_service_account_is_granted_through_the_services_own_sid() {
        for a in ["NT AUTHORITY\\NetworkService", "NT AUTHORITY\\Network Service", "networkservice", " NT AUTHORITY\\LocalService ", "LocalService", "S-1-5-20", "S-1-5-19"] {
            assert_eq!(grantee("OmniIngestService", a), "NT SERVICE\\OmniIngestService", "{a}");
        }
        for a in [crate::DEFAULT_SERVICE_ACCOUNT, "NEWSROOM\\svc_ingest", ".\\svc-test"] {
            assert_eq!(grantee("OmniIngestService", a), a, "{a}");
        }
    }

    #[test]
    fn local_system_needs_no_grant_and_every_other_account_does() {
        for a in [None, Some("LocalSystem"), Some("NT AUTHORITY\\SYSTEM"), Some(" ")] {
            assert!(!needs_self_control(a), "{a:?}");
        }
        for a in [crate::DEFAULT_SERVICE_ACCOUNT, "NT AUTHORITY\\NetworkService", "NEWSROOM\\svc_ingest"] {
            assert!(needs_self_control(Some(a)), "{a}");
        }
    }
}
