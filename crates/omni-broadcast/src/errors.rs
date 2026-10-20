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
    /// The page itself does not exist (HTTP 404/410): a mistyped or decorated
    /// link, or a deleted article. Waiting and the browser cannot help.
    PageNotFound,
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
    /// The page's video is copy-protected (EME/DRM); no download can get it.
    DrmProtected,
    /// Looked at twice, with a diagnostic pass: the page has no video.
    NoVideoOnPage,
    /// The page has a video we could not get. A human takes it from here.
    VideoUnresolved,
    /// MCR looked at a video found automatically and turned it down (P10.5).
    RejectedByMcr,
    /// A held video was not approved in time and its file was deleted.
    ApprovalExpired,
    /// The file that waited for approval is not the one that was approved.
    HeldFileChanged,
    /// The automatic repair ran out of time or attempts.
    RepairExhausted,
}

impl ErrorCode {
    /// Every code. A new variant goes here too, or `from_code` cannot read it.
    pub const ALL: [ErrorCode; 31] = [
        Self::UnsupportedUrl,
        Self::Http403,
        Self::LoginRequired,
        Self::GeoBlocked,
        Self::PrivateOrRemoved,
        Self::PageNotFound,
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
        Self::DrmProtected,
        Self::NoVideoOnPage,
        Self::VideoUnresolved,
        Self::RejectedByMcr,
        Self::ApprovalExpired,
        Self::HeldFileChanged,
        Self::RepairExhausted,
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
            Self::PageNotFound => "PAGE_NOT_FOUND",
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
            Self::DrmProtected => "DRM_PROTECTED",
            Self::NoVideoOnPage => "NO_VIDEO_ON_PAGE",
            Self::VideoUnresolved => "VIDEO_UNRESOLVED",
            Self::RejectedByMcr => "REJECTED_BY_MCR",
            Self::ApprovalExpired => "APPROVAL_EXPIRED",
            Self::HeldFileChanged => "HELD_FILE_CHANGED",
            Self::RepairExhausted => "REPAIR_EXHAUSTED",
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
                // A bare 403 is a CDN or platform refusing for now: YouTube
                // turned two videos away for a few minutes on 2026-10-06
                // that downloaded fine minutes later. Geo blocks and logins
                // have codes of their own and are not retried.
                | Self::Http403
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
            Self::Http403 => "Η πηγή αρνήθηκε τη λήψη. Έγιναν αυτόματες επαναλήψεις χωρίς επιτυχία· πατήστε «Δοκιμή ξανά» αργότερα ή δώστε άλλο σύνδεσμο.",
            Self::LoginRequired => "Ο ιστότοπος ζητά σύνδεση (login) για αυτό το βίντεο. Επικολλήστε άλλον σύνδεσμο για το ίδιο βίντεο· αν συμβαίνει συχνά με αυτόν τον ιστότοπο, ζητήστε από τον διαχειριστή να προσθέσει cookies.",
            Self::GeoBlocked => "Το βίντεο δεν είναι διαθέσιμο από την Ελλάδα.",
            Self::PrivateOrRemoved => "Το βίντεο είναι ιδιωτικό ή έχει αφαιρεθεί. Ζητήστε άλλον σύνδεσμο.",
            Self::PageNotFound => "Η σελίδα δεν υπάρχει (404). Ελέγξτε τον σύνδεσμο: συχνά έχει κολλήσει μια λέξη στο τέλος του (π.χ. «-ΒΙΝΤΕΟ»). Διορθώστε τον ή ζητήστε τον σωστό.",
            Self::LiveStream => "Ζωντανή μετάδοση. Περιμένετε να γίνει διαθέσιμη η εγγραφή.",
            Self::Network => "Πρόβλημα δικτύου. Γίνεται αυτόματη επανάληψη.",
            Self::NoStreamFound => "Δεν βρέθηκε βίντεο στη σελίδα. Ανοίξτε τον σύνδεσμο: αν δεν έχει βίντεο, πατήστε «Αφαίρεση»· αν έχει, επικολλήστε παρακάτω τον σύνδεσμο της ανάρτησης με το βίντεο (YouTube, Instagram, Facebook…).",
            Self::ProbeFailed => "Το αρχείο που κατέβηκε δεν διαβάζεται. Πιθανώς ατελής λήψη.",
            Self::TranscodeFailed => "Απέτυχε η μετατροπή. Δείτε την «Αιτία» και το «Ιστορικό» σε αυτή την κάρτα· αν επαναλαμβάνεται, ενημερώστε τον διαχειριστή.",
            Self::RewrapFailed => "Απέτυχε η ενθυλάκωση σε MXF. Πατήστε «Δοκιμή ξανά»· αν επαναλαμβάνεται, ενημερώστε τον διαχειριστή.",
            Self::ComplianceFailed => "Το αρχείο δεν πληροί τις προδιαγραφές εκπομπής και ΔΕΝ παραδόθηκε.",
            Self::DeliveryFailed => "Απέτυχε η παράδοση στον φάκελο του Dalet. Ελέγξτε το δίκτυο και τα δικαιώματα.",
            Self::LowDisk => "Ανεπαρκής χώρος στον δίσκο. Ενημερώστε τον διαχειριστή.",
            Self::SourceTooLong => "Το βίντεο υπερβαίνει το επιτρεπτό όριο διάρκειας.",
            Self::ExtractTimeout | Self::DownloadTimeout => "Λήξη χρόνου κατά τη λήψη.",
            Self::TranscodeTimeout => "Λήξη χρόνου κατά τη μετατροπή.",
            Self::RewrapTimeout => "Λήξη χρόνου κατά την ενθυλάκωση.",
            Self::DeliverTimeout => "Λήξη χρόνου κατά την παράδοση.",
            Self::LeaseExpired => "Η εργασία διακόπηκε και δεν ολοκληρώθηκε μετά από επανειλημμένες προσπάθειες.",
            Self::ManualDownload => "Σύνδεσμος μεταφοράς αρχείων (WeTransfer κ.λπ.). Κατεβάστε το αρχείο από τον σύνδεσμο, βάλτε το στο Dalet και πατήστε «Το έβαλα στο Dalet».",
            Self::PipelineFailed => "Απρόβλεπτο σφάλμα. Δείτε την «Αιτία» και το «Ιστορικό» σε αυτή την κάρτα, και δοκιμάστε ξανά ή δώστε άλλον σύνδεσμο.",
            Self::DrmProtected => "Το βίντεο έχει προστασία αντιγραφής (DRM) και δεν μπορεί να κατέβει. Ζητήστε το αρχείο από την πηγή ή βάλτε το στο Dalet με άλλο τρόπο και πατήστε «Το έβαλα στο Dalet».",
            Self::NoVideoOnPage => "Η σελίδα δεν έχει βίντεο (ελέγχθηκε δύο φορές). Αν εννοούσαν άλλον σύνδεσμο, επικολλήστε τον στο «Χρήση αυτού του συνδέσμου»· αλλιώς πατήστε «Αφαίρεση».",
            Self::VideoUnresolved => "Η σελίδα έχει βίντεο αλλά δεν κατέβηκε αυτόματα. Ανοίξτε τον σύνδεσμο και κατεβάστε το χειροκίνητα, ή δοκιμάστε ξανά αργότερα.",
            Self::RejectedByMcr => "Το MCR απέρριψε το βίντεο που βρέθηκε αυτόματα. Κατεβάστε το σωστό χειροκίνητα ή δώστε άλλον σύνδεσμο.",
            Self::ApprovalExpired => "Κανείς δεν ενέκρινε το βίντεο μέσα σε 24 ώρες και το αρχείο σβήστηκε. Πατήστε «Δοκιμή ξανά» αν το χρειάζεστε ακόμα.",
            Self::HeldFileChanged => "Το αρχείο που περίμενε έγκριση άλλαξε ή χάθηκε και ΔΕΝ παραδόθηκε. Πατήστε «Δοκιμή ξανά».",
            Self::RepairExhausted => "Η αυτόματη επισκευή δεν βρήκε το βίντεο μέσα στον χρόνο της. Ανοίξτε τον σύνδεσμο και κατεβάστε το χειροκίνητα.",
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
    ///
    /// YouTube: never. Its page holds sign-in and recommendation links, not
    /// a stream the browser can see, and sniffing it after a 403 on
    /// 2026-10-06 "found" a video id read out of a `/ServiceLogin` link,
    /// which then failed under a misleading error.
    pub fn should_sniff(&self, url: &str) -> bool {
        // A live video is live in the browser too: the sniffer would only find
        // the same stream, or a sibling that is already its own job (P1.18).
        if *self == Self::LiveStream {
            return false;
        }
        // A page that does not exist has no player to find.
        if *self == Self::PageNotFound || crate::downloader::is_youtube(url) {
            return false;
        }
        // yt-dlp's extractor read the post and found no video in it; the
        // post's page would only offer the browser someone else's.
        if *self == Self::NoStreamFound && crate::downloader::is_video_platform(url) {
            return false;
        }
        !crate::downloader::is_video_platform(url) || self.should_try_sniffer()
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The card's hint for a login wall, naming the site (P3.5): no jar yet, or a
/// jar that no longer gets in. Greek, for MCR; the action is the admin's.
pub fn login_hint_el(domain: &str, jar_exists: bool) -> String {
    if jar_exists {
        format!("Τα cookies για {domain} έληξαν· ανεβάστε νέα")
    } else {
        format!("Χρειάζονται cookies για {domain}: Διαχείριση → Cookies → Σύνδεση")
    }
}

/// What the card tells the operator for a job that failed with `code`: the
/// code's own hint, except a login wall on a video platform (or a site that has
/// a jar), which names the site and says whether a jar is already there. A
/// login wall on a news article does not name the article's site: the wall was
/// the embedded player's, which this link does not tell, so the generic hint stands.
pub fn hint_for_job(code: ErrorCode, url: &str, repo: &omni_core::repository::Repository) -> String {
    if code == ErrorCode::LoginRequired {
        if let Some(domain) = omni_core::cookies::jar_domain_for_url(url) {
            let jar = repo.has_cookie_jar(&domain).unwrap_or(false);
            if jar || crate::downloader::is_video_platform(url) {
                return login_hint_el(&domain, jar);
            }
        }
    }
    code.hint_el().to_string()
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
    // YouTube's temporary rate limit also says "Video unavailable. This content isn't
    // available, try again later.": waiting helps, the video is not gone. A bot check or a login wall stays
    // a login wall.
    // Whole phrases only: a bare "rate-limit" also matches an article slug in the
    // URL ("…/ecb-rate-limit-story"), which would hide UNSUPPORTED_URL or a 404.
    if !(s.contains("sign in to confirm") || s.contains("login required") || s.contains("use --cookies"))
        && (s.contains("try again later")
            || s.contains("rate-limited by")
            || s.contains("rate limited by")
            || s.contains("rate-limit exceeded")
            || s.contains("rate limit exceeded")
            || s.contains("rate-limit reached")
            || s.contains("rate limit reached"))
    {
        return ErrorCode::Network;
    }
    // YouTube says "Video unavailable. …" for a removed video and "This video
    // is unavailable" for an id it does not know (a mistyped link).
    if s.contains("private video")
        || s.contains("video unavailable")
        || s.contains("this video is unavailable")
        || s.contains("has been removed")
        || s.contains("account associated with this video has been terminated")
        || s.contains("this video is no longer available")
        || s.contains("removed by the uploader")
    {
        return ErrorCode::PrivateOrRemoved;
    }
    if s.contains("is a live event")
        || (s.contains("does not pass filter") && s.contains("is_live"))
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
        // X, Facebook and Instagram when the link needs the station's login (P3.5).
        || s.contains("this post is unavailable")
        || s.contains("age-restricted")
        || s.contains("you need to log in")
        || s.contains("you must log in")
    {
        return ErrorCode::LoginRequired;
    }
    if s.contains("http error 403") || s.contains("403 forbidden") {
        return ErrorCode::Http403;
    }
    // A post with photos, or text only: yt-dlp's extractor read the post and
    // it has no video. Not unexpected, and not something to sniff for.
    if s.contains("no video could be found in this tweet") || s.contains("there's no video in this post") {
        return ErrorCode::NoStreamFound;
    }
    // The page, not a media segment: "Unable to download webpage: HTTP Error
    // 404". A 404 on video data can be an expired signed URL and is not this.
    if s.contains("unable to download webpage: http error 404") || s.contains("unable to download webpage: http error 410") {
        return ErrorCode::PageNotFound;
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

    /// YouTube's rate limit says "Video unavailable" too: it must wait, not read as a gone video.
    #[test]
    fn a_rate_limit_is_network_and_a_gone_video_still_is_not() {
        let cases: &[(&str, ErrorCode)] = &[
            ("ERROR: [youtube] abc: Video unavailable. This content isn't available, try again later.", ErrorCode::Network),
            ("ERROR: [youtube] abc: The current session has been rate-limited by YouTube for up to an hour", ErrorCode::Network),
            ("ERROR: [youtube] abc: Video unavailable", ErrorCode::PrivateOrRemoved),
            ("ERROR: [youtube] abc: This video is unavailable", ErrorCode::PrivateOrRemoved),
            ("ERROR: [youtube] abc: Private video. Sign in if you've been granted access", ErrorCode::PrivateOrRemoved),
            ("ERROR: [youtube] abc: Sign in to confirm you're not a bot. Use --cookies-from-browser", ErrorCode::LoginRequired),
            ("ERROR: [twitter] 123: Rate limit exceeded", ErrorCode::Network),
            // A slug that merely says "rate-limit" is not a rate limit.
            ("ERROR: Unsupported URL: https://news.example/ecb-rate-limit-story", ErrorCode::UnsupportedUrl),
            (
                "ERROR: [generic] new-rate-limit-rules: Unable to download webpage: HTTP Error 404: Not Found",
                ErrorCode::PageNotFound,
            ),
        ];
        for (text, want) in cases {
            assert_eq!(classify_download_error(text), *want, "{text}");
        }
    }

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
                // Verbatim (job 82, 2026-10-07): a link with "-ΒΙΝΤΕΟ" glued on.
                "ERROR: [generic] -ΒΙΝΤΕΟ: Unable to download webpage: HTTP Error 404: Not Found (caused by <HTTPError 404: Not Found>)",
                ErrorCode::PageNotFound,
            ),
            (
                // A media segment, not the page: not a dead link.
                "ERROR: unable to download video data: HTTP Error 404: Not Found",
                ErrorCode::PipelineFailed,
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
                // A reply that has no video: final (not retried), and not a
                // sniffer case either (`should_sniff` refuses NO_STREAM_FOUND
                // on a platform post). It used to read "unexpected error".
                "ERROR: [twitter] 1900000000000000011: No video could be found in this tweet",
                ErrorCode::NoStreamFound,
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
    fn a_mistyped_youtube_id_asks_for_another_link() {
        // yt-dlp's other wording, for an id YouTube does not know. Verbatim
        // from the dev instance, 2026-10-07: it fell through to
        // PIPELINE_FAILED, and the card said «Απρόβλεπτο σφάλμα» instead of
        // asking for another link.
        let code = classify_download_error("ERROR: [youtube] zzzzzzzzzz0: This video is unavailable");
        assert_eq!(code, ErrorCode::PrivateOrRemoved);
        assert!(!code.is_retryable());
        // Not verbatim: the same words with a geo reason stay a geo block,
        // which is why the geo phrases are matched first.
        assert_eq!(
            classify_download_error("ERROR: [youtube] zzzzzzzzzz0: This video is unavailable in your country"),
            ErrorCode::GeoBlocked
        );
    }

    /// P3.5: what the login-wall messages of X, Facebook and Instagram look like.
    #[test]
    fn login_walls_are_recognised_and_ordinary_failures_are_not() {
        for msg in [
            "ERROR: [twitter] 1: NSFW tweet requires authentication. Use --cookies, --cookies-from-browser, --username and --password",
            "ERROR: [twitter] 2: This post is unavailable",
            "ERROR: [Instagram] abc: Requested content is not available, rate-limit reached or login required. Use --cookies",
            "ERROR: [facebook] 3: You must log in to continue",
            "ERROR: [facebook] 4: You need to log in to access this content",
            "ERROR: [youtube] 5: Sign in to confirm your age. This video may be inappropriate for some users.",
            "ERROR: [youtube] 6: This video is age-restricted",
        ] {
            assert_eq!(classify_download_error(msg), ErrorCode::LoginRequired, "{msg}");
        }
        for (msg, not) in [
            ("ERROR: [twitter] 1: No video could be found in this tweet", ErrorCode::LoginRequired),
            ("ERROR: unable to download video data: HTTP Error 403: Forbidden", ErrorCode::LoginRequired),
            ("ERROR: [youtube] 1: Private video. Sign in if you've been granted access", ErrorCode::LoginRequired),
            ("ERROR: Unsupported URL: https://www.example.gr/a", ErrorCode::LoginRequired),
        ] {
            assert_ne!(classify_download_error(msg), not, "{msg}");
        }
        assert!(!ErrorCode::LoginRequired.is_retryable());
    }

    #[test]
    fn a_url_that_merely_says_nsfw_is_not_a_login_wall() {
        assert_eq!(
            classify_download_error("ERROR: [generic] https://example.org/nsfw-clip: HTTP Error 403: Forbidden"),
            ErrorCode::Http403
        );
        // X's own NSFW message is still caught by its other words.
        assert_eq!(
            classify_download_error("ERROR: [twitter] 1: NSFW tweet requires authentication. Use --cookies"),
            ErrorCode::LoginRequired
        );
    }

    #[test]
    fn the_login_hint_names_the_site_and_says_whether_a_jar_exists() {
        assert_eq!(login_hint_el("x.com", false), "Χρειάζονται cookies για x.com: Διαχείριση → Cookies → Σύνδεση");
        assert_eq!(login_hint_el("x.com", true), "Τα cookies για x.com έληξαν· ανεβάστε νέα");

        let dir = tempfile::tempdir().unwrap();
        let repo = omni_core::repository::Repository::new(dir.path().join("omni.db")).unwrap();
        let url = "https://twitter.com/someone/status/1";
        // An article on a news site is not the site that asked for a login.
        let article = "https://www.news.example/story/12345";
        assert_eq!(hint_for_job(ErrorCode::LoginRequired, article, &repo), ErrorCode::LoginRequired.hint_el());
        assert_eq!(hint_for_job(ErrorCode::LoginRequired, url, &repo), login_hint_el("x.com", false));
        let jar = omni_core::cookies::parse_netscape("x.com", ".x.com	TRUE	/	TRUE	0	n	value-123456
").unwrap();
        repo.put_cookie_jar("x.com", &jar, "admin", chrono::Utc::now()).unwrap();
        assert_eq!(hint_for_job(ErrorCode::LoginRequired, url, &repo), login_hint_el("x.com", true));
        // A site the admin gave a jar is named even when it is not a known platform.
        let jar = omni_core::cookies::parse_netscape("news.example", ".news.example\tTRUE\t/\tTRUE\t0\tn\tvalue-123456\n").unwrap();
        repo.put_cookie_jar("news.example", &jar, "admin", chrono::Utc::now()).unwrap();
        assert_eq!(hint_for_job(ErrorCode::LoginRequired, article, &repo), login_hint_el("news.example", true));
        // Another code keeps its own hint.
        assert_eq!(hint_for_job(ErrorCode::GeoBlocked, url, &repo), ErrorCode::GeoBlocked.hint_el());
    }

    #[test]
    fn only_genuinely_transient_failures_are_retried() {
        for retryable in [
            ErrorCode::Network,
            ErrorCode::Http403,
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
    fn a_live_stream_is_never_sniffed_and_other_codes_on_an_article_are_as_before() {
        let article = "https://www.portal.example/story/1";
        assert!(!ErrorCode::LiveStream.should_sniff(article));
        assert!(!ErrorCode::LiveStream.should_sniff("https://x.com/i/status/1"));
        for code in [ErrorCode::UnsupportedUrl, ErrorCode::PrivateOrRemoved, ErrorCode::LoginRequired, ErrorCode::Http403] {
            assert!(code.should_sniff(article), "{code}");
        }
    }

    #[test]
    fn a_video_skipped_by_the_live_filter_is_a_live_stream() {
        // yt-dlp 2026.08.19 prints this on stdout and exits 0 with no file.
        let line = "[download] Some title does not pass filter (!is_live & live_status!=?is_upcoming), skipping ..";
        assert_eq!(classify_download_error(line), ErrorCode::LiveStream);
        assert_ne!(classify_download_error("[download] t does not pass filter (duration>5), skipping .."), ErrorCode::LiveStream);
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
        // Now classified as such ("No video could be found in this tweet").
        assert!(!ErrorCode::NoStreamFound.should_sniff(reply));
        // A page that does not exist has no player either (P3.10).
        assert!(!ErrorCode::PageNotFound.should_sniff(article));
        // Transient: retried, not sniffed.
        assert!(!ErrorCode::Network.should_sniff(reply));
        assert!(ErrorCode::Http403.should_sniff(reply));
        // YouTube's own extractor is the only way to its video.
        for yt in ["https://www.youtube.com/watch?v=Eksg5qHdZho&t=89s", "https://youtu.be/l0lr6MGpMu4"] {
            for code in [ErrorCode::Http403, ErrorCode::UnsupportedUrl, ErrorCode::PipelineFailed] {
                assert!(!code.should_sniff(yt), "{code} sniffed {yt}");
            }
        }
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
            ErrorCode::DrmProtected,
            ErrorCode::NoVideoOnPage,
            ErrorCode::VideoUnresolved,
            ErrorCode::RejectedByMcr,
            ErrorCode::ApprovalExpired,
            ErrorCode::HeldFileChanged,
            ErrorCode::RepairExhausted,
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
        assert_eq!(ErrorCode::DrmProtected.as_str(), "DRM_PROTECTED");
        assert_eq!(ErrorCode::NoVideoOnPage.as_str(), "NO_VIDEO_ON_PAGE");
        assert_eq!(ErrorCode::VideoUnresolved.as_str(), "VIDEO_UNRESOLVED");
    }

    #[test]
    fn the_page_evidence_codes_are_terminal_and_listed_once() {
        for c in [ErrorCode::DrmProtected, ErrorCode::NoVideoOnPage, ErrorCode::VideoUnresolved] {
            assert!(!c.is_retryable(), "{c} must not be retried");
            assert!(!c.should_try_sniffer());
            assert_eq!(ErrorCode::ALL.iter().filter(|x| **x == c).count(), 1);
        }
        assert_eq!(ErrorCode::ALL.len(), 31);
        let unique: std::collections::HashSet<_> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(unique.len(), 31);
    }

    #[test]
    fn the_approval_codes_are_terminal_stable_and_greek() {
        let cases = [
            (ErrorCode::RejectedByMcr, "REJECTED_BY_MCR"),
            (ErrorCode::ApprovalExpired, "APPROVAL_EXPIRED"),
            (ErrorCode::HeldFileChanged, "HELD_FILE_CHANGED"),
            (ErrorCode::RepairExhausted, "REPAIR_EXHAUSTED"),
        ];
        for (c, text) in cases {
            assert_eq!(c.as_str(), text);
            assert_eq!(ErrorCode::from_code(text), Some(c));
            assert!(!c.is_retryable(), "{c} must not be retried");
            assert!(!c.should_try_sniffer());
            assert_eq!(ErrorCode::ALL.iter().filter(|x| **x == c).count(), 1);
            assert!(c.hint_el().chars().any(|ch| ('\u{0370}'..='\u{03FF}').contains(&ch)), "{c}");
        }
        assert!(ErrorCode::LowDisk.is_retryable(), "a refused hold is retried like a full disk");
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
