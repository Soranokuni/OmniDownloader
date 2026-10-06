//! Error codes, retry policy and operator hints (plan P1.9).
//!
//! Free-text error messages cannot drive behaviour. Before this, every failure
//! looked the same to the daemon: a job whose CDN dropped a connection and a job
//! whose video was deleted both landed in review with a sentence of yt-dlp
//! output, and an operator had to read stderr to tell which was worth retrying.
//!
//! An [`ErrorCode`] decides three things: whether a retry can possibly help, how
//! long to wait, and what the MCR card tells the operator to do about it — in
//! Greek, because that is who reads it at two in the morning.

use std::fmt;

use chrono::Duration;
use serde::{Deserialize, Serialize};

/// Stable, machine-readable failure reasons.
///
/// The string forms appear in the database, the API and the panels; treat them
/// as a public contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// yt-dlp has no extractor for this page. The sniffer is the next step.
    UnsupportedUrl,
    /// The origin refused the direct fetch. Sniffed session context often fixes it.
    Http403,
    /// The site wants an authenticated session.
    LoginRequired,
    GeoBlocked,
    PrivateOrRemoved,
    /// A live stream, with no VOD yet.
    LiveStream,
    /// Timeouts, resets, DNS. The one class where waiting genuinely helps.
    Network,
    /// The sniffer found nothing playable on the page.
    NoStreamFound,
    ProbeFailed,
    TranscodeFailed,
    RewrapFailed,
    ComplianceFailed,
    DeliveryFailed,
    LowDisk,
    SourceTooLong,
    ExtractTimeout,
    DownloadTimeout,
    TranscodeTimeout,
    RewrapTimeout,
    DeliverTimeout,
    /// The worker died without releasing the job and its attempts ran out.
    LeaseExpired,
    /// A file-locker link a human has to fetch.
    ManualDownload,
    /// Anything unclassified. Always goes to a human.
    PipelineFailed,
}

impl ErrorCode {
    /// Every code. A new variant goes here too, or `from_code` cannot read it.
    pub const ALL: [ErrorCode; 23] = [
        Self::UnsupportedUrl,
        Self::Http403,
        Self::LoginRequired,
        Self::GeoBlocked,
        Self::PrivateOrRemoved,
        Self::LiveStream,
        Self::Network,
        Self::NoStreamFound,
        Self::ProbeFailed,
        Self::TranscodeFailed,
        Self::RewrapFailed,
        Self::ComplianceFailed,
        Self::DeliveryFailed,
        Self::LowDisk,
        Self::SourceTooLong,
        Self::ExtractTimeout,
        Self::DownloadTimeout,
        Self::TranscodeTimeout,
        Self::RewrapTimeout,
        Self::DeliverTimeout,
        Self::LeaseExpired,
        Self::ManualDownload,
        Self::PipelineFailed,
    ];

    /// The code stored on a job, back as a code.
    pub fn from_code(code: &str) -> Option<ErrorCode> {
        Self::ALL.iter().copied().find(|c| c.as_str() == code.trim())
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnsupportedUrl => "UNSUPPORTED_URL",
            Self::Http403 => "HTTP_403",
            Self::LoginRequired => "LOGIN_REQUIRED",
            Self::GeoBlocked => "GEO_BLOCKED",
            Self::PrivateOrRemoved => "PRIVATE_OR_REMOVED",
            Self::LiveStream => "LIVE_STREAM",
            Self::Network => "NETWORK",
            Self::NoStreamFound => "NO_STREAM_FOUND",
            Self::ProbeFailed => "PROBE_FAILED",
            Self::TranscodeFailed => "TRANSCODE_FAILED",
            Self::RewrapFailed => "REWRAP_FAILED",
            Self::ComplianceFailed => "COMPLIANCE_FAILED",
            Self::DeliveryFailed => "DELIVERY_FAILED",
            Self::LowDisk => "LOW_DISK",
            Self::SourceTooLong => "SOURCE_TOO_LONG",
            Self::ExtractTimeout => "EXTRACT_TIMEOUT",
            Self::DownloadTimeout => "DOWNLOAD_TIMEOUT",
            Self::TranscodeTimeout => "TRANSCODE_TIMEOUT",
            Self::RewrapTimeout => "REWRAP_TIMEOUT",
            Self::DeliverTimeout => "DELIVER_TIMEOUT",
            Self::LeaseExpired => "LEASE_EXPIRED",
            Self::ManualDownload => "MANUAL_DOWNLOAD",
            Self::PipelineFailed => "PIPELINE_FAILED",
        }
    }

    /// Whether retrying the same job unchanged could plausibly succeed.
    ///
    /// Deliberately conservative. Retrying a deleted video wastes a worker slot
    /// and delays real work; worse, it delays the moment a human is told the
    /// link is dead, which is the only thing that can actually fix it.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Network
                | Self::DeliveryFailed
                | Self::LowDisk
                | Self::DownloadTimeout
                | Self::ExtractTimeout
                | Self::DeliverTimeout
        )
    }

    /// Backoff before the nth retry (1-based).
    ///
    /// A share that is briefly unreachable or a CDN hiccup clears in seconds; a
    /// full disk needs someone to act, so the waits lengthen quickly rather than
    /// hammering.
    pub fn backoff(&self, attempt: i32) -> Duration {
        match self {
            Self::LowDisk => Duration::minutes(10),
            _ => match attempt.max(1) {
                1 => Duration::seconds(30),
                2 => Duration::minutes(2),
                _ => Duration::minutes(5),
            },
        }
    }

    /// What the MCR card tells the operator to do, in Greek.
    pub fn hint_el(&self) -> &'static str {
        match self {
            Self::UnsupportedUrl => "Δεν αναγνωρίζεται η σελίδα. Δοκιμάστε απευθείας σύνδεσμο βίντεο.",
            Self::Http403 => "Η πηγή μπλοκάρει την απευθείας λήψη. Δοκιμάστε ξανά ή δώστε άλλο σύνδεσμο.",
            Self::LoginRequired => "Απαιτείται σύνδεση. Ανεβάστε cookies για αυτόν τον ιστότοπο (Διαχείριση → Cookies).",
            Self::GeoBlocked => "Το βίντεο δεν είναι διαθέσιμο από την Ελλάδα.",
            Self::PrivateOrRemoved => "Το βίντεο είναι ιδιωτικό ή έχει αφαιρεθεί. Ζητήστε άλλον σύνδεσμο.",
            Self::LiveStream => "Ζωντανή μετάδοση. Περιμένετε να γίνει διαθέσιμη η εγγραφή.",
            Self::Network => "Πρόβλημα δικτύου. Γίνεται αυτόματη επανάληψη.",
            Self::NoStreamFound => "Δεν βρέθηκε βίντεο στη σελίδα. Ανοίξτε τον σύνδεσμο: αν δεν έχει βίντεο, πατήστε «Αφαίρεση»· αν έχει, επικολλήστε παρακάτω τον σύνδεσμο της ανάρτησης με το βίντεο (YouTube, Instagram, Facebook…).",
            Self::ProbeFailed => "Το αρχείο που κατέβηκε δεν διαβάζεται. Πιθανώς ατελής λήψη.",
            Self::TranscodeFailed => "Απέτυχε η μετατροπή. Δείτε το σφάλμα στις λεπτομέρειες.",
            Self::RewrapFailed => "Απέτυχε η ενθυλάκωση σε MXF.",
            Self::ComplianceFailed => "Το αρχείο δεν πληροί τις προδιαγραφές εκπομπής και ΔΕΝ παραδόθηκε.",
            Self::DeliveryFailed => "Απέτυχε η παράδοση στον φάκελο του Dalet. Ελέγξτε το δίκτυο και τα δικαιώματα.",
            Self::LowDisk => "Ανεπαρκής χώρος στον δίσκο.",
            Self::SourceTooLong => "Το βίντεο υπερβαίνει το επιτρεπτό όριο διάρκειας.",
            Self::ExtractTimeout | Self::DownloadTimeout => "Λήξη χρόνου κατά τη λήψη.",
            Self::TranscodeTimeout => "Λήξη χρόνου κατά τη μετατροπή.",
            Self::RewrapTimeout => "Λήξη χρόνου κατά την ενθυλάκωση.",
            Self::DeliverTimeout => "Λήξη χρόνου κατά την παράδοση.",
            Self::LeaseExpired => "Η εργασία διακόπηκε και δεν ολοκληρώθηκε μετά από επανειλημμένες προσπάθειες.",
            Self::ManualDownload => "Σύνδεσμος μεταφοράς αρχείων. Κατεβάστε το αρχείο και ανεβάστε το εδώ.",
            Self::PipelineFailed => "Απρόβλεπτο σφάλμα. Δείτε τις λεπτομέρειες της εργασίας.",
        }
    }

    /// Whether the sniffer should be tried after this failure.
    pub fn should_try_sniffer(&self) -> bool {
        matches!(self, Self::UnsupportedUrl | Self::Http403 | Self::NoStreamFound)
    }

    /// Whether to open `url` in the browser after yt-dlp failed on it with
    /// this code (plan P3.1, rule 5).
    ///
    /// A news article: yes — yt-dlp failing there is the normal route to the
    /// embedded player. A platform post: only where the sniffer can help.
    /// yt-dlp's extractor knows which video belongs to the post; the page does
    /// not. An X reply without a video of its own plays the thread parent's,
    /// and sniffing it delivered someone else's video under the reply's name.
    pub fn should_sniff(&self, url: &str) -> bool {
        !crate::downloader::is_video_platform(url) || self.should_try_sniffer()
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify a download failure from the tool's stderr tail.
///
/// Order matters: the specific messages are matched before the generic ones,
/// because yt-dlp's text for a private video also mentions "unavailable" and
/// several messages mention "error".
pub fn classify_download_error(stderr: &str) -> ErrorCode {
    let s = stderr.to_lowercase();

    // Most specific first.
    // yt-dlp words this several ways depending on the extractor:
    // "is not available in your country", "The uploader has not made this
    // video available in your country", "blocked it in your country".
    if s.contains("in your country")
        || s.contains("not available from your location")
        || s.contains("in your location")
        || s.contains("geo restricted")
        || s.contains("geo-restricted")
    {
        return ErrorCode::GeoBlocked;
    }
    if s.contains("private video")
        || s.contains("video unavailable")
        || s.contains("has been removed")
        || s.contains("account associated with this video has been terminated")
        || s.contains("this video is no longer available")
        || s.contains("removed by the uploader")
    {
        return ErrorCode::PrivateOrRemoved;
    }
    if s.contains("is a live event")
        || s.contains("live stream")
        || s.contains("this live event will begin")
        || s.contains("premieres in")
    {
        return ErrorCode::LiveStream;
    }
    if s.contains("sign in to confirm")
        || s.contains("login required")
        || s.contains("requires authentication")
        || s.contains("use --cookies")
        || s.contains("cookies-from-browser")
        || s.contains("members-only")
        || s.contains("private account")
    {
        return ErrorCode::LoginRequired;
    }
    if s.contains("http error 403") || s.contains("403 forbidden") {
        return ErrorCode::Http403;
    }
    if s.contains("unsupported url") || s.contains("no suitable extractor") {
        return ErrorCode::UnsupportedUrl;
    }
    // The platform's API answered with an empty or non-JSON body: X does this
    // for a minute or two under load (trial run 2026-09-25, five posts in a
    // row, all fine a few minutes later). Waiting helps; if the extractor is
    // really broken, the retries run out and the nightly yt-dlp update is the
    // fix.
    if s.contains("failed to parse json") {
        return ErrorCode::Network;
    }
    // Network last: its phrases are generic and appear inside other messages.
    if s.contains("timed out")
        || s.contains("timeout")
        || s.contains("connection reset")
        || s.contains("connection aborted")
        || s.contains("connection refused")
        || s.contains("temporary failure in name resolution")
        || s.contains("failed to resolve")
        || s.contains("getaddrinfo")
        || s.contains("network is unreachable")
        || s.contains("remote end closed connection")
        || s.contains("http error 5")
        || s.contains("incomplete read")
    {
        return ErrorCode::Network;
    }

    ErrorCode::PipelineFailed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real yt-dlp messages. Getting these wrong means either an operator is
    /// told to wait for a video that will never come back, or a transient CDN
    /// hiccup is escalated to a human at 2am.
    #[test]
    fn real_yt_dlp_messages_classify_correctly() {
        let cases: &[(&str, ErrorCode)] = &[
            (
                "ERROR: [youtube] dQw4w9WgXcQ: Private video. Sign in if you've been granted access to this video",
                ErrorCode::PrivateOrRemoved,
            ),
            (
                "ERROR: [youtube] abc: Video unavailable. This video has been removed by the uploader",
                ErrorCode::PrivateOrRemoved,
            ),
            (
                "ERROR: [youtube] xyz: The uploader has not made this video available in your country",
                ErrorCode::GeoBlocked,
            ),
            (
                "ERROR: [youtube] abc: This live event will begin in 3 hours",
                ErrorCode::LiveStream,
            ),
            (
                "ERROR: [twitter] 123: No video could be found in this tweet; Sign in to confirm you're not a bot. Use --cookies",
                ErrorCode::LoginRequired,
            ),
            (
                "ERROR: unable to download video data: HTTP Error 403: Forbidden",
                ErrorCode::Http403,
            ),
            (
                "ERROR: Unsupported URL: https://www.example.gr/article/12345",
                ErrorCode::UnsupportedUrl,
            ),
            (
                "ERROR: unable to download video data: <urlopen error [Errno 110] Connection timed out>",
                ErrorCode::Network,
            ),
            (
                "ERROR: unable to download: ConnectionResetError(104, 'Connection reset by peer')",
                ErrorCode::Network,
            ),
            (
                // Verbatim from the 2026-09-25 trial; the same posts fetched
                // fine minutes later.
                "ERROR: [twitter] 1900000000000000001: Failed to parse JSON (caused by JSONDecodeError(\"Expecting value in '': line 1 column 1 (char 0)\")); please report this issue on  https://github.com/yt-dlp/yt-dlp/issues?q= , filling out the appropriate issue template. Confirm you are on the latest version using  yt-dlp -U",
                ErrorCode::Network,
            ),
            (
                // A reply that has no video: final, not a sniffer case.
                "ERROR: [twitter] 1900000000000000011: No video could be found in this tweet",
                ErrorCode::PipelineFailed,
            ),
            (
                "ERROR: [generic] Requested format is not available",
                ErrorCode::PipelineFailed,
            ),
        ];

        for (stderr, expected) in cases {
            assert_eq!(
                classify_download_error(stderr),
                *expected,
                "misclassified: {stderr}"
            );
        }
    }

    #[test]
    fn a_removed_video_is_not_mistaken_for_a_network_problem() {
        // "Video unavailable" also reads like a transient failure. Retrying a
        // deleted video for three attempts wastes worker slots and, worse,
        // delays telling a journalist their link is dead -- the only action that
        // can actually fix it.
        let code = classify_download_error("ERROR: Video unavailable");
        assert_eq!(code, ErrorCode::PrivateOrRemoved);
        assert!(!code.is_retryable());
    }

    #[test]
    fn only_genuinely_transient_failures_are_retried() {
        for retryable in [
            ErrorCode::Network,
            ErrorCode::DeliveryFailed,
            ErrorCode::LowDisk,
            ErrorCode::DownloadTimeout,
        ] {
            assert!(retryable.is_retryable(), "{retryable} should retry");
        }
        for terminal in [
            ErrorCode::PrivateOrRemoved,
            ErrorCode::GeoBlocked,
            ErrorCode::LiveStream,
            ErrorCode::LoginRequired,
            ErrorCode::ComplianceFailed,
            ErrorCode::NoStreamFound,
            ErrorCode::UnsupportedUrl,
            ErrorCode::SourceTooLong,
        ] {
            assert!(!terminal.is_retryable(), "{terminal} should not retry");
        }
    }

    #[test]
    fn a_failed_compliance_check_is_never_retried_automatically() {
        // Retrying would produce the same non-compliant file and eventually
        // exhaust the attempts, hiding a real format regression behind a
        // generic "gave up" instead of putting the report in front of a human.
        assert!(!ErrorCode::ComplianceFailed.is_retryable());
        assert!(ErrorCode::ComplianceFailed
            .hint_el()
            .contains("ΔΕΝ παραδόθηκε"));
    }

    #[test]
    fn backoff_lengthens_and_low_disk_waits_for_a_human() {
        assert!(ErrorCode::Network.backoff(1) < ErrorCode::Network.backoff(2));
        assert!(ErrorCode::Network.backoff(2) < ErrorCode::Network.backoff(3));
        // A full disk will not clear in 30 seconds; somebody has to act.
        assert_eq!(ErrorCode::LowDisk.backoff(1), Duration::minutes(10));
    }

    #[test]
    fn the_sniffer_is_tried_exactly_where_it_can_help() {
        assert!(ErrorCode::UnsupportedUrl.should_try_sniffer());
        assert!(ErrorCode::Http403.should_try_sniffer());
        // No point opening a browser for a video that no longer exists.
        assert!(!ErrorCode::PrivateOrRemoved.should_try_sniffer());
        assert!(!ErrorCode::GeoBlocked.should_try_sniffer());
        assert!(!ErrorCode::LiveStream.should_try_sniffer());
    }

    #[test]
    fn a_platform_post_is_sniffed_only_where_the_sniffer_can_help() {
        let reply = "https://x.com/SomeReader/status/1900000000000000011";
        let article = "https://www.portal.gr/article/1";
        // yt-dlp said the reply has no video. Its page plays the thread
        // parent's video, and that must not be delivered under this job.
        assert!(!ErrorCode::PipelineFailed.should_sniff(reply));
        // Transient: retried, not sniffed.
        assert!(!ErrorCode::Network.should_sniff(reply));
        assert!(ErrorCode::Http403.should_sniff(reply));
        // An article is sniffed whatever yt-dlp said.
        assert!(ErrorCode::PipelineFailed.should_sniff(article));
        assert!(ErrorCode::UnsupportedUrl.should_sniff(article));
    }

    #[test]
    fn every_code_has_an_actionable_greek_hint() {
        // The MCR card is read at 2am by someone who did not write this code.
        for code in [
            ErrorCode::UnsupportedUrl,
            ErrorCode::Http403,
            ErrorCode::LoginRequired,
            ErrorCode::GeoBlocked,
            ErrorCode::PrivateOrRemoved,
            ErrorCode::LiveStream,
            ErrorCode::Network,
            ErrorCode::NoStreamFound,
            ErrorCode::ProbeFailed,
            ErrorCode::TranscodeFailed,
            ErrorCode::RewrapFailed,
            ErrorCode::ComplianceFailed,
            ErrorCode::DeliveryFailed,
            ErrorCode::LowDisk,
            ErrorCode::SourceTooLong,
            ErrorCode::LeaseExpired,
            ErrorCode::ManualDownload,
            ErrorCode::PipelineFailed,
        ] {
            let hint = code.hint_el();
            assert!(hint.len() > 20, "{code} has no useful hint");
            assert!(
                hint.chars().any(|c| ('\u{0370}'..='\u{03FF}').contains(&c)),
                "{code}'s hint is not in Greek: {hint}"
            );
        }
    }

    #[test]
    fn code_strings_are_stable() {
        // These live in the database, the API and the panels' filters.
        assert_eq!(ErrorCode::Http403.as_str(), "HTTP_403");
        assert_eq!(ErrorCode::ComplianceFailed.as_str(), "COMPLIANCE_FAILED");
        assert_eq!(ErrorCode::NoStreamFound.as_str(), "NO_STREAM_FOUND");
        assert_eq!(ErrorCode::ManualDownload.as_str(), "MANUAL_DOWNLOAD");
    }

    /// The MCR desk shows a stored code's Greek hint (plan P7.1); every code
    /// must read back, each once.
    #[test]
    fn every_stored_code_reads_back() {
        let mut seen = std::collections::HashSet::new();
        for c in ErrorCode::ALL {
            assert!(seen.insert(c.as_str()), "{} listed twice", c.as_str());
            assert_eq!(ErrorCode::from_code(c.as_str()), Some(c));
            assert!(!c.hint_el().is_empty());
        }
        assert_eq!(ErrorCode::from_code("NOT_A_CODE"), None);
    }
}
