//! When the repair ladder may run (plan P10.11a). Pure: the worker decides
//! from the failure it already has, nothing here touches a job.

use omni_core::config::RepairMode;

use crate::errors::ErrorCode;

/// What kind of job a failure belongs to, as far as repair is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// A news page (or any web link) that is not one of the below.
    WebPage,
    /// A video attached to a mail (`attachment://`).
    Attachment,
    /// A file-locker or manually handled link.
    Locker,
    /// A video platform link (YouTube, X, ...): yt-dlp's own extractors.
    Platform,
    /// A video found inside an article (a sibling job): says nothing about the page.
    ArticleSibling,
    /// Anything else (not an http(s) address).
    Other,
}

/// The ladder runs only after the normal path failed AND the page showed a
/// video that could not be resolved, never for a platform post, an
/// attachment, a locker, an article's sibling, a site whose breaker is open,
/// or a video MCR already rejected.
pub fn eligible(
    mode: RepairMode,
    code: ErrorCode,
    kind: JobKind,
    site_breaker_open: bool,
    already_rejected_same: bool,
) -> bool {
    mode != RepairMode::Off
        && code == ErrorCode::VideoUnresolved
        && kind == JobKind::WebPage
        && !site_breaker_open
        && !already_rejected_same
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [RepairMode; 3] = [RepairMode::Off, RepairMode::Shadow, RepairMode::Hold];
    const KINDS: [JobKind; 6] = [
        JobKind::WebPage,
        JobKind::Attachment,
        JobKind::Locker,
        JobKind::Platform,
        JobKind::ArticleSibling,
        JobKind::Other,
    ];

    /// Every code x mode x kind x flags: true for exactly one shape of failure.
    #[test]
    fn only_an_unresolved_video_on_a_web_page_with_repair_on_is_eligible() {
        let mut yes = 0;
        for code in ErrorCode::ALL {
            for mode in MODES {
                for kind in KINDS {
                    for breaker in [false, true] {
                        for rejected in [false, true] {
                            let got = eligible(mode, code, kind, breaker, rejected);
                            let want = mode != RepairMode::Off
                                && code == ErrorCode::VideoUnresolved
                                && kind == JobKind::WebPage
                                && !breaker
                                && !rejected;
                            assert_eq!(got, want, "{code:?} {mode:?} {kind:?} breaker={breaker} rejected={rejected}");
                            yes += got as usize;
                        }
                    }
                }
            }
        }
        assert_eq!(yes, 2, "Shadow and Hold, nothing else");
    }

    #[test]
    fn the_codes_that_are_not_a_missed_video_never_qualify() {
        for code in [
            ErrorCode::NoVideoOnPage,
            ErrorCode::NoStreamFound,
            ErrorCode::Http403,
            ErrorCode::DrmProtected,
            ErrorCode::RepairExhausted,
            ErrorCode::RejectedByMcr,
        ] {
            assert!(!eligible(RepairMode::Hold, code, JobKind::WebPage, false, false), "{code:?}");
        }
        assert!(eligible(RepairMode::Hold, ErrorCode::VideoUnresolved, JobKind::WebPage, false, false));
    }
}
