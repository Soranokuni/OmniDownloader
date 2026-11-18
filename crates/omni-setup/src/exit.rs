//! Setup's exit codes (stable: deployment tools and the docs read them) and the
//! code an elevated child uses to ask its unelevated parent to open the browser.

use std::fmt;

use omni_cli::install::{Cancelled, Refusal};
use omni_core::answer_file::AnswerFileError;

use crate::args::UsageError;
use crate::payload::PayloadError;

/// What Setup exits with. The numbers never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Ok = 0,
    /// The install or uninstall failed.
    Failed = 1,
    /// The command line is wrong.
    Usage = 2,
    /// Not elevated, and `/quiet` may not ask for it.
    NeedsElevation = 3,
    /// The payload is missing where one is needed, corrupt, truncated or unsafe.
    BadPayload = 4,
    /// Refused before anything on the machine was changed.
    Refused = 5,
    /// UAC was declined or the operator answered no.
    Cancelled = 6,
    /// An upgrade did not come up; the previous version is back and running.
    RolledBack = 7,
}

impl ExitCode {
    pub const ALL: [ExitCode; 8] = [
        ExitCode::Ok,
        ExitCode::Failed,
        ExitCode::Usage,
        ExitCode::NeedsElevation,
        ExitCode::BadPayload,
        ExitCode::Refused,
        ExitCode::Cancelled,
        ExitCode::RolledBack,
    ];

    pub fn code(self) -> u32 {
        self as u8 as u32
    }

    pub fn from_code(code: u32) -> Option<ExitCode> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }
}

/// An error that carries the exit code it ends Setup with. Its text is the
/// text of what went wrong.
#[derive(Debug)]
pub struct Failure {
    pub code: ExitCode,
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// `e` with the exit code `code`, its text kept.
pub fn coded(code: ExitCode, e: anyhow::Error) -> anyhow::Error {
    Failure { code, message: format!("{e:#}") }.into()
}

/// A [`Failure`] from text.
pub fn fail(code: ExitCode, message: impl Into<String>) -> anyhow::Error {
    Failure { code, message: message.into() }.into()
}

/// The exit code an error ends Setup with.
pub fn exit_code_for(e: &anyhow::Error) -> ExitCode {
    for cause in e.chain() {
        if let Some(f) = cause.downcast_ref::<Failure>() {
            return f.code;
        }
        if cause.downcast_ref::<Cancelled>().is_some() {
            return ExitCode::Cancelled;
        }
        if cause.downcast_ref::<UsageError>().is_some() {
            return ExitCode::Usage;
        }
        if cause.downcast_ref::<PayloadError>().is_some() {
            return ExitCode::BadPayload;
        }
        if cause.downcast_ref::<Refusal>().is_some() || cause.downcast_ref::<AnswerFileError>().is_some() {
            return ExitCode::Refused;
        }
    }
    ExitCode::Failed
}

/// What an elevated child's exit code means to its unelevated parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildResult {
    Exit(ExitCode),
    /// A new installation succeeded: the parent opens the admin page (an
    /// elevated process would open it with the administrator's token).
    OpenAdmin { tls: bool, port: u16 },
}

const OPEN_HTTP: u32 = 0x20000;
const OPEN_HTTPS: u32 = 0x30000;

/// The child's exit code for "open `scheme://127.0.0.1:<port>/admin`".
pub fn encode_open(tls: bool, port: u16) -> u32 {
    (if tls { OPEN_HTTPS } else { OPEN_HTTP }) + port as u32
}

/// The parent's reading of a child's exit code. Anything unknown is a failure.
pub fn decode_child_exit(code: u32) -> ChildResult {
    if let Some(c) = ExitCode::from_code(code) {
        return ChildResult::Exit(c);
    }
    let open = |base: u32, tls: bool| {
        (code > base && code <= base + u16::MAX as u32).then(|| ChildResult::OpenAdmin { tls, port: (code - base) as u16 })
    };
    open(OPEN_HTTP, false).or_else(|| open(OPEN_HTTPS, true)).unwrap_or(ChildResult::Exit(ExitCode::Failed))
}

/// The address the parent opens.
pub fn admin_url(tls: bool, port: u16) -> String {
    format!("{}://127.0.0.1:{port}/admin", if tls { "https" } else { "http" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_the_documented_numbers() {
        let numbers: Vec<u32> = ExitCode::ALL.iter().map(|c| c.code()).collect();
        assert_eq!(numbers, [0, 1, 2, 3, 4, 5, 6, 7]);
        for c in ExitCode::ALL {
            assert_eq!(ExitCode::from_code(c.code()), Some(c));
        }
        assert_eq!(ExitCode::from_code(8), None);
    }

    #[test]
    fn the_child_s_open_request_round_trips_and_plain_codes_decode_as_themselves() {
        for port in [1u16, 8080, 65535] {
            for tls in [false, true] {
                assert_eq!(decode_child_exit(encode_open(tls, port)), ChildResult::OpenAdmin { tls, port });
            }
        }
        assert_eq!(encode_open(false, 8080), 0x20000 + 8080);
        assert_eq!(encode_open(true, 443), 0x30000 + 443);
        for c in ExitCode::ALL {
            assert_eq!(decode_child_exit(c.code()), ChildResult::Exit(c));
        }
        // Port 0 and anything else unknown are failures, never a page to open.
        for odd in [0x20000, 0x30000, 0x40000, 8, 0xC000_0005, u32::MAX] {
            assert_eq!(decode_child_exit(odd), ChildResult::Exit(ExitCode::Failed), "{odd:#x}");
        }
        assert_eq!(admin_url(true, 8443), "https://127.0.0.1:8443/admin");
    }

    #[test]
    fn errors_map_to_their_exit_codes() {
        use anyhow::anyhow;
        let refusal = Refusal { notice: vec![], message: "no".into() };
        let cases: Vec<(anyhow::Error, ExitCode)> = vec![
            (anyhow!("disk full"), ExitCode::Failed),
            (anyhow::Error::new(UsageError("bad".into())), ExitCode::Usage),
            (anyhow::Error::new(PayloadError::Corrupt), ExitCode::BadPayload),
            (anyhow::Error::new(refusal), ExitCode::Refused),
            (anyhow::Error::new(AnswerFileError::NoVersion), ExitCode::Refused),
            (anyhow::Error::new(Cancelled), ExitCode::Cancelled),
            (anyhow::Error::new(Cancelled).context("asking for the password"), ExitCode::Cancelled),
            // The text alone is no longer read as a cancel.
            (anyhow!("Installation cancelled."), ExitCode::Failed),
            (fail(ExitCode::NeedsElevation, "elevate"), ExitCode::NeedsElevation),
            (fail(ExitCode::RolledBack, "back"), ExitCode::RolledBack),
            (coded(ExitCode::Refused, anyhow!("Cannot install: x")), ExitCode::Refused),
            // Through a context, too.
            (anyhow::Error::new(PayloadError::Truncated).context("reading Setup"), ExitCode::BadPayload),
        ];
        for (e, want) in cases {
            assert_eq!(exit_code_for(&e), want, "{e:#}");
        }
        assert_eq!(coded(ExitCode::Refused, anyhow!("Cannot install: x")).to_string(), "Cannot install: x");
    }
}
