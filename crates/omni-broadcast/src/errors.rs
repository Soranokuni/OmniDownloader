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
    /// The link, or a stream found behind it, points at a loopback, private or reserved address (P2.12).
    NonPublicAddress,
    /// A delivery was interrupted and nobody can tell whether its file reached
    /// the watchfolder (P11.2b): MCR checks Dalet before anything is retried.
    DeliveryUnconfirmed,
    /// The platform is refusing our requests for now: its bot check, HTTP 429,
    /// "try again later" (P11.4). Waiting, longer than for a network fault, helps.
    RateLimited,
}

impl ErrorCode {
    /// Every code. A new variant goes here too, or `from_code` cannot read it.
    pub const ALL: [ErrorCode; 34] = [
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
        Self::NonPublicAddress,
        Self::DeliveryUnconfirmed,
        Self::RateLimited,
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
            Self::NonPublicAddress => "NON_PUBLIC_ADDRESS",
            Self::DeliveryUnconfirmed => "DELIVERY_UNCONFIRMED",
            Self::RateLimited => "RATE_LIMITED",
        }
    }

    /// Whether retrying the same job unchanged could plausibly succeed.
    ///
    /// Deliberately conservative. Retrying a deleted video wastes a worker slot
    /// and delays real work; worse, it delays the moment a human is told the
    /// link is dead, which is the only thing that can actually fix it.
    pub fn is_retryable(&self) -> bool {
        !self.retry_delays().is_empty()
    }

    /// The waits before each automatic retry of a job that failed with this
    /// code (P11.3a): the nth retry waits the nth. Empty for a code no wait
    /// can fix. A newsroom video is needed in minutes: a CDN hiccup or a share
    /// that is briefly unreachable clears in seconds, and four tries over
    /// about 17 minutes are patient enough without hiding a real fault.
    /// HTTP_403 keeps two (30 s, 2 min).
    ///
    /// LOW_DISK waits 10 minutes three times, then goes to review: the clip
    /// itself may need more room than the volume has, and every retry is a
    /// full run. A disk that is really full is handled by the worker loop,
    /// which leases nothing while the disk check is red.
    pub fn retry_delays(&self) -> &'static [Duration] {
        match self {
            // A bare 403 is a CDN or platform refusing for now: YouTube
            // turned two videos away for a few minutes on 2026-10-06
            // that downloaded fine minutes later. Geo blocks and logins
            // have codes of their own and are not retried. Two tries only:
            // a 403 that survives them is far more often a real block, and
            // on a news page the evidence and repair route takes it from there.
            Self::Http403 => &HTTP_403_DELAYS,
            Self::Network
                | Self::DeliveryFailed
                | Self::DownloadTimeout
                | Self::ExtractTimeout
                | Self::DeliverTimeout => &TRANSIENT_DELAYS,
            Self::LowDisk => &LOW_DISK_DELAYS,
            // A platform's rate limit or bot check (P11.4) lasts minutes, not
            // seconds: 2, 5, 10 and 15 minutes, while the pacer holds the
            // platform's other jobs for two minutes after each refusal.
            Self::RateLimited => &RATE_LIMITED_DELAYS,
            // A stream still live, or a premiere not yet started (P11.7): its
            // recording usually exists an hour or two later. Checked again 10,
            // 30, 60 and 120 minutes after the job was queued, then review.
            Self::LiveStream => &LIVE_STREAM_DELAYS,
            _ => &[],
        }
    }

    /// The `max_attempts` a job retried for this code shows: the first run
    /// and one per wait. `None` for a code that is not retried.
    pub fn retry_max_attempts(&self) -> Option<i32> {
        let delays = self.retry_delays();
        (!delays.is_empty()).then(|| delays.len() as i32 + 1)
    }

    /// Whether a job that stopped retrying with this code shows the
    /// "exhausted" hint instead of its own (P11.3a): only the codes whose own
    /// hint describes a passing condition. DELIVERY_FAILED (check the share),
    /// HTTP_403 (already says the retries failed) and LOW_DISK (tell the
    /// administrator) keep their own. LIVE_STREAM (P11.7) has an exhausted
    /// text of its own ([`stopped_hint_el`]).
    pub fn uses_exhausted_hint(&self) -> bool {
        matches!(
            self,
            Self::Network
                | Self::DownloadTimeout
                | Self::ExtractTimeout
                | Self::DeliverTimeout
                | Self::RateLimited
                | Self::LiveStream
        )
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
            Self::NonPublicAddress => "Ο σύνδεσμος δείχνει σε εσωτερική διεύθυνση· δεν κατεβαίνει αυτόματα.",
            Self::DeliveryUnconfirmed => "Η παράδοση διακόπηκε και δεν είναι βέβαιο αν το αρχείο έφτασε. Ελέγξτε πρώτα στο Dalet αν υπάρχει ήδη: αν υπάρχει, πατήστε «Το έβαλα στο Dalet»· αν όχι, πατήστε «Δοκιμή ξανά».",
            Self::RateLimited => "Ο ιστότοπος περιορίζει προσωρινά τις λήψεις. Γίνεται αυτόματη επανάληψη.",
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
        // A platform refusing us for now (P11.4) refuses the browser's
        // requests too; the retry is what helps.
        if *self == Self::RateLimited {
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

/// 30 s, 2 min, 5 min, 10 min: the transient codes' waits (P11.3a).
static TRANSIENT_DELAYS: [Duration; 4] =
    [Duration::seconds(30), Duration::minutes(2), Duration::minutes(5), Duration::minutes(10)];

/// 30 s, 2 min: HTTP_403's waits, its budget before P11.3a.
static HTTP_403_DELAYS: [Duration; 2] = [Duration::seconds(30), Duration::minutes(2)];

/// 10 min three times: LOW_DISK's waits, then review.
static LOW_DISK_DELAYS: [Duration; 3] = [Duration::minutes(10), Duration::minutes(10), Duration::minutes(10)];

/// 2, 5, 10, 15 min: RATE_LIMITED's waits (P11.4), then review.
static RATE_LIMITED_DELAYS: [Duration; 4] =
    [Duration::minutes(2), Duration::minutes(5), Duration::minutes(10), Duration::minutes(15)];

/// 10, 20, 30, 60 min: LIVE_STREAM's waits (P11.7), so the checks fall 10,
/// 30, 60 and 120 minutes after the first run. The last is scheduled when the
/// job is about an hour old, inside [`retry_after`]'s age limit; its failure
/// has no wait left and goes to review.
static LIVE_STREAM_DELAYS: [Duration; 4] =
    [Duration::minutes(10), Duration::minutes(20), Duration::minutes(30), Duration::minutes(60)];

/// The review card's text for a stream that was still live after its last
/// check (P11.7).
pub const LIVE_STREAM_EXHAUSTED_EL: &str = "Η μετάδοση ήταν ακόμη ζωντανή (ή δεν είχε ξεκινήσει) σε όλους τους αυτόματους ελέγχους. Δοκιμάστε ξανά όταν ανέβει η εγγραφή, ή δώστε άλλον σύνδεσμο.";

/// How long a job that failed with `code` waits before its automatic retry,
/// or `None` when it goes to review instead (P11.3a). `attempts` is the
/// job's count after the failed run (1 for the first; a value below 1 reads
/// as 1), and `age` how long ago it was queued, or last started afresh by
/// MCR; an age below zero (a clock that stepped back) reads as 0.
///
/// A code no wait can fix is never retried. Otherwise the job retries while
/// it has a wait left for this attempt, and only while it is younger than
/// `max_age`.
pub fn retry_after(code: ErrorCode, attempts: i32, age: Duration, max_age: Duration) -> Option<Duration> {
    if age.max(Duration::zero()) >= max_age {
        return None;
    }
    let nth = usize::try_from(attempts.max(1) - 1).ok()?;
    code.retry_delays().get(nth).copied()
}

/// The card's hint for a job that stopped retrying with a code whose own
/// hint says a retry is under way ([`ErrorCode::uses_exhausted_hint`],
/// P11.3a). A job that ran once was not retried at all: the age limit
/// stopped it.
pub fn exhausted_hint_el(attempts: i32) -> String {
    if attempts <= 1 {
        "Δεν τα κατάφερε και δεν ξαναδοκιμάστηκε αυτόματα, γιατί η εργασία είναι παλιά. Πατήστε «Δοκιμή ξανά» ή επικολλήστε άλλον σύνδεσμο.".to_string()
    } else {
        format!("Δεν τα κατάφερε μετά από {attempts} προσπάθειες. Πατήστε «Δοκιμή ξανά» ή επικολλήστε άλλον σύνδεσμο.")
    }
}

/// The hint of a job that stopped retrying `code` (one whose own hint says a
/// retry is under way, [`ErrorCode::uses_exhausted_hint`]) after `attempts`,
/// for the desk's card and the job's history alike, so the two cannot drift.
/// A rate limit that outlasted its retries on a video platform (or on a site
/// that has a jar) is followed by the same site's login hint a login wall
/// gets: the station's cookies are what usually gets past it. A stream still
/// live after its checks has its own text (P11.7), but only when job `job_id`
/// was re-queued as live (its history has a LIVE_STREAM retry, P11.7b; its
/// `max_attempts` cannot tell, NETWORK and RATE_LIMITED give the same 5); one
/// never checked again as live (past the age limit, or retried for another
/// cause before) gets the exhausted text: nothing will check it again, so it
/// must not say to wait (P11.7b2).
pub fn stopped_hint_el(
    code: ErrorCode,
    attempts: i32,
    job_id: i64,
    url: &str,
    repo: &omni_core::repository::Repository,
) -> String {
    if code == ErrorCode::LiveStream {
        let checked_as_live = attempts > 1 && repo.was_requeued_with(job_id, code.as_str()).unwrap_or(false);
        return if checked_as_live { LIVE_STREAM_EXHAUSTED_EL.to_string() } else { exhausted_hint_el(attempts) };
    }
    let exhausted = exhausted_hint_el(attempts);
    if code == ErrorCode::RateLimited {
        let login = hint_for_job(ErrorCode::LoginRequired, url, repo);
        if login != ErrorCode::LoginRequired.hint_el() {
            return format!("{exhausted} {login}");
        }
    }
    exhausted
}

/// The codes [`code_in_text`] looks for in a failure's error chain, first
/// match wins. The specific ones come before the generic: a chain that says
/// both NO_STREAM_FOUND and NETWORK is a page without a video, and one that
/// says both LOW_DISK and REWRAP_FAILED is a full disk. The two stage
/// failures come last, so any more exact code a stage attached wins.
/// RATE_LIMITED (P11.4) is appended after them: only a failed yt-dlp run
/// attaches it, a job that has it is never sniffed and never reaches a stage,
/// so no chain carries it with another code.
pub const CLASSIFIED: &[ErrorCode] = &[
    ErrorCode::ManualDownload,
    ErrorCode::HeldFileChanged,
    ErrorCode::ComplianceFailed,
    ErrorCode::ProbeFailed,
    ErrorCode::LoginRequired,
    ErrorCode::GeoBlocked,
    ErrorCode::PrivateOrRemoved,
    ErrorCode::PageNotFound,
    ErrorCode::LiveStream,
    ErrorCode::Http403,
    ErrorCode::UnsupportedUrl,
    ErrorCode::NoStreamFound,
    ErrorCode::DownloadTimeout,
    ErrorCode::TranscodeTimeout,
    ErrorCode::RewrapTimeout,
    ErrorCode::DeliveryFailed,
    ErrorCode::LowDisk,
    ErrorCode::Network,
    ErrorCode::TranscodeFailed,
    ErrorCode::RewrapFailed,
    ErrorCode::RateLimited,
];

/// Every other code, and why the text classifier does not look for it. A new
/// code goes in one of the two lists, or a test fails.
pub const NOT_CLASSIFIED: &[(ErrorCode, &str)] = &[
    (ErrorCode::SourceTooLong, "set by the benchmark only; no job path raises it"),
    (ErrorCode::ExtractTimeout, "set by the benchmark only; no job path raises it"),
    (ErrorCode::DeliverTimeout, "nothing raises it; a failed delivery is DELIVERY_FAILED"),
    (ErrorCode::LeaseExpired, "set by the reaper in SQL; the engine's lost lease is recognised by type, not text"),
    (ErrorCode::PipelineFailed, "the fallback"),
    (ErrorCode::DrmProtected, "set directly by the worker from the failed page's evidence"),
    (ErrorCode::NoVideoOnPage, "set directly by the worker from the failed page's evidence"),
    (ErrorCode::VideoUnresolved, "set directly by the worker from the failed page's evidence or the repair ladder"),
    (ErrorCode::RejectedByMcr, "set by an MCR action in SQL"),
    (ErrorCode::ApprovalExpired, "set by the held-video expiry in SQL"),
    (ErrorCode::RepairExhausted, "set directly by the repair ladder"),
    (ErrorCode::NonPublicAddress, "set directly by the egress check, which ends the job itself"),
    (ErrorCode::DeliveryUnconfirmed, "set directly when a delivery was interrupted and its result is unknown"),
];

/// The first code of [`CLASSIFIED`] that `text` names as a whole token: the
/// characters either side of it are not `A-Z`, `0-9` or `_`. So
/// "MY_NETWORK_X" and "HTTP_4031" name nothing, and "x:NETWORK" names NETWORK.
pub fn code_in_text(text: &str) -> Option<ErrorCode> {
    CLASSIFIED.iter().copied().find(|code| names_token(text, code.as_str()))
}

fn names_token(text: &str, token: &str) -> bool {
    let bytes = text.as_bytes();
    let part = |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_';
    text.match_indices(token).any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + token.len()).copied();
        !before.is_some_and(part) && !after.is_some_and(part)
    })
}

/// Whether a tool's failure text says the disk is full: bmxtranswrap's failed
/// write check (job 68), FFmpeg's ENOSPC, or Windows' own wording.
pub fn disk_full_text(text: &str) -> bool {
    let s = text.to_ascii_lowercase();
    if s.contains("write(data, size) == size")
        || s.contains("no space left on device")
        || s.contains("there is not enough space on the disk")
    {
        return true;
    }
    // ERROR_DISK_FULL, and not a longer number that starts with 112.
    s.match_indices("os error 112")
        .any(|(at, m)| !s.as_bytes().get(at + m.len()).is_some_and(|b| b.is_ascii_digit()))
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
        if let Some((domain, jar)) = login_wall_site(url, |d| repo.has_cookie_jar(d).unwrap_or(false)) {
            return login_hint_el(&domain, jar);
        }
    }
    code.hint_el().to_string()
}

/// The site whose login wall stopped a `LOGIN_REQUIRED` job on `url`, and
/// whether it has a jar (`has_jar`): the link's jar domain when it is a video
/// platform or a site that has a jar; `None` for a news article's embedded
/// player, whose site the link does not tell. [`hint_for_job`] and the desk's
/// «waiting for a login» card (P11.12) name the same site.
pub fn login_wall_site(url: &str, has_jar: impl Fn(&str) -> bool) -> Option<(String, bool)> {
    let domain = omni_core::cookies::jar_domain_for_url(url)?;
    let jar = has_jar(&domain);
    (jar || crate::downloader::is_video_platform(url)).then_some((domain, jar))
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
    // The platform refusing us for now (P11.4), before the login wall and the
    // gone video: YouTube's bot check («Sign in to confirm you're not a bot.
    // Use --cookies…», typographic apostrophe in production) came after a burst
    // of downloads and passed one at a time, and its temporary limit also says
    // "Video unavailable. This content isn't available, try again later.".
    // Waiting helps; neither is a login wall nor a removed video.
    // Whole phrases only: a bare "rate-limit" also matches an article slug in the
    // URL ("…/ecb-rate-limit-story"), which would hide UNSUPPORTED_URL or a 404.
    // The bot check is a passing refusal on YouTube only (its extractors tag
    // their errors `[youtube]`, `[youtube:tab]`, …). On X the same words come
    // with a post that never opens without the station's login: that stays a
    // login wall, sent to review at once with the cookie advice.
    if (s.contains("not a bot") && s.contains("[youtube"))
        || s.contains("try again later")
        || s.contains("rate-limited by")
        || s.contains("rate limited by")
        || s.contains("rate-limit exceeded")
        || s.contains("rate limit exceeded")
        || s.contains("http error 429")
        || s.contains("too many requests")
    {
        return ErrorCode::RateLimited;
    }
    // Instagram words its limit and its login wall as one ("rate-limit reached
    // or login required. Use --cookies"): that stays a login wall.
    if !(s.contains("sign in to confirm") || s.contains("login required") || s.contains("use --cookies"))
        && (s.contains("rate-limit reached") || s.contains("rate limit reached"))
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
    /// P11.4: a rate limit or bot check is RATE_LIMITED (it was NETWORK, and the
    /// bot check LOGIN_REQUIRED); a limit worded with a login wall stays NETWORK.
    #[test]
    fn a_rate_limit_is_network_and_a_gone_video_still_is_not() {
        let cases: &[(&str, ErrorCode)] = &[
            ("ERROR: [youtube] abc: Video unavailable. This content isn't available, try again later.", ErrorCode::RateLimited),
            ("ERROR: [youtube] abc: The current session has been rate-limited by YouTube for up to an hour", ErrorCode::RateLimited),
            ("ERROR: [youtube] abc: Video unavailable", ErrorCode::PrivateOrRemoved),
            ("ERROR: [youtube] abc: This video is unavailable", ErrorCode::PrivateOrRemoved),
            ("ERROR: [youtube] abc: Private video. Sign in if you've been granted access", ErrorCode::PrivateOrRemoved),
            ("ERROR: [youtube] abc: Sign in to confirm you're not a bot. Use --cookies-from-browser", ErrorCode::RateLimited),
            ("ERROR: [twitter] 123: Rate limit exceeded", ErrorCode::RateLimited),
            ("ERROR: [generic] abc: Rate limit reached", ErrorCode::Network),
            ("ERROR: unable to download video data: HTTP Error 429: Too Many Requests", ErrorCode::RateLimited),
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
                // P11.4 review: the bot check is a passing refusal on YouTube
                // only. On X it does not clear without the station's login, so
                // it stays a login wall (review at once, with the cookie hint)
                // instead of four retries that hold every X job.
                "ERROR: [twitter] 123: No video could be found in this tweet; Sign in to confirm you're not a bot. Use --cookies",
                ErrorCode::LoginRequired,
            ),
            (
                // Verbatim from production (2026-11), typographic apostrophe.
                "ERROR: [youtube] abc123: Sign in to confirm you’re not a bot. Use --cookies-from-browser or --cookies for the authentication. See https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp for how to manually pass your cookies",
                ErrorCode::RateLimited,
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
            // P11.4: the bot check names cookies but is a temporary refusal.
            ("ERROR: [youtube] 7: Sign in to confirm you’re not a bot. Use --cookies-from-browser or --cookies", ErrorCode::LoginRequired),
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
            ErrorCode::ExtractTimeout,
            ErrorCode::DeliverTimeout,
            ErrorCode::RateLimited,
            ErrorCode::LiveStream,
        ] {
            assert!(retryable.is_retryable(), "{retryable} should retry");
        }
        assert_eq!(
            ErrorCode::ALL.iter().filter(|c| c.is_retryable()).count(),
            9,
            "the seven codes as before, RATE_LIMITED, and LIVE_STREAM (P11.7)"
        );
        for terminal in [
            ErrorCode::PrivateOrRemoved,
            ErrorCode::GeoBlocked,
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
        let hours2 = Duration::minutes(120);
        let wait = |n| retry_after(ErrorCode::Network, n, Duration::zero(), hours2).unwrap();
        assert!(wait(1) < wait(2) && wait(2) < wait(3) && wait(3) < wait(4));
        // A full disk will not clear in 30 seconds; somebody has to act.
        assert_eq!(retry_after(ErrorCode::LowDisk, 1, Duration::zero(), hours2), Some(Duration::minutes(10)));
        assert_eq!(ErrorCode::LowDisk.retry_max_attempts(), Some(4));
        assert_eq!(ErrorCode::Network.retry_max_attempts(), Some(5));
        assert_eq!(ErrorCode::PrivateOrRemoved.retry_max_attempts(), None);
    }

    /// P11.3a: the waits per code, over every code. Five transient codes wait
    /// 30 s, 2 min, 5 min, 10 min; HTTP_403 30 s, 2 min; LOW_DISK 10 min three
    /// times; RATE_LIMITED 2, 5, 10, 15 min; LIVE_STREAM (P11.7) 10, 20, 30,
    /// 60 min; every other code has none.
    #[test]
    fn every_code_has_its_retry_waits() {
        let transient = [
            Duration::seconds(30),
            Duration::minutes(2),
            Duration::minutes(5),
            Duration::minutes(10),
        ];
        for c in ErrorCode::ALL {
            let want: &[Duration] = match c {
                ErrorCode::Http403 => &[Duration::seconds(30), Duration::minutes(2)],
                ErrorCode::Network
                | ErrorCode::DownloadTimeout
                | ErrorCode::ExtractTimeout
                | ErrorCode::DeliverTimeout
                | ErrorCode::DeliveryFailed => &transient,
                ErrorCode::LowDisk => &[Duration::minutes(10), Duration::minutes(10), Duration::minutes(10)],
                ErrorCode::RateLimited => &[Duration::minutes(2), Duration::minutes(5), Duration::minutes(10), Duration::minutes(15)],
                ErrorCode::LiveStream => &[Duration::minutes(10), Duration::minutes(20), Duration::minutes(30), Duration::minutes(60)],
                _ => &[],
            };
            assert_eq!(c.retry_delays(), want, "{c}");
            assert_eq!(c.is_retryable(), !want.is_empty(), "{c}");
            assert_eq!(c.retry_max_attempts(), (!want.is_empty()).then(|| want.len() as i32 + 1), "{c}");
        }
        assert_eq!(ErrorCode::Http403.retry_max_attempts(), Some(3), "HTTP_403 keeps its old budget");
        // P11.7 changed only LIVE_STREAM: two codes pinned by name.
        assert_eq!(ErrorCode::Network.retry_delays(), &transient, "NETWORK's waits changed");
        assert_eq!(ErrorCode::Network.retry_max_attempts(), Some(5));
        assert_eq!(ErrorCode::Http403.retry_delays(), &[Duration::seconds(30), Duration::minutes(2)], "HTTP_403's waits changed");
        let max = Duration::minutes(120);
        let got: Vec<_> = (1..=3).map(|n| retry_after(ErrorCode::Http403, n, Duration::zero(), max)).collect();
        assert_eq!(got, vec![Some(Duration::seconds(30)), Some(Duration::minutes(2)), None]);
    }

    #[test]
    fn a_network_failure_retries_four_times_then_goes_to_review() {
        let max = Duration::minutes(120);
        let young = Duration::minutes(1);
        let got: Vec<_> = (1..=5).map(|n| retry_after(ErrorCode::Network, n, young, max)).collect();
        assert_eq!(
            got,
            vec![
                Some(Duration::seconds(30)),
                Some(Duration::minutes(2)),
                Some(Duration::minutes(5)),
                Some(Duration::minutes(10)),
                None
            ]
        );
        // A count below 1 (a waiting video's lease spends none) is the first try.
        assert_eq!(retry_after(ErrorCode::Network, 0, young, max), Some(Duration::seconds(30)));
        assert_eq!(retry_after(ErrorCode::Network, -3, young, max), Some(Duration::seconds(30)));
        assert_eq!(retry_after(ErrorCode::DeliveryFailed, 40, young, max), None);
    }

    #[test]
    fn no_retry_once_the_job_is_two_hours_old() {
        let max = Duration::minutes(120);
        assert_eq!(retry_after(ErrorCode::Network, 1, Duration::minutes(119), max), Some(Duration::seconds(30)));
        assert_eq!(retry_after(ErrorCode::Network, 1, Duration::minutes(121), max), None);
        assert_eq!(retry_after(ErrorCode::Network, 1, max, max), None, "at the limit it stops");
        assert_eq!(retry_after(ErrorCode::Http403, 2, Duration::minutes(121), max), None);
        // A created_at in the future (the clock stepped back) is age 0: retried, and still bounded.
        assert_eq!(retry_after(ErrorCode::Network, 1, Duration::minutes(-500), max), Some(Duration::seconds(30)));
        assert_eq!(retry_after(ErrorCode::Network, 5, Duration::minutes(-500), max), None);
    }

    /// A clip too big for the volume fails LOW_DISK on every run: three waits
    /// of 10 minutes, then review, and not at all past the age limit.
    #[test]
    fn a_full_disk_retries_three_times_and_nothing_else_retries_on_its_own() {
        let max = Duration::minutes(120);
        let young = Duration::minutes(1);
        let got: Vec<_> = (1..=4).map(|n| retry_after(ErrorCode::LowDisk, n, young, max)).collect();
        let ten = Some(Duration::minutes(10));
        assert_eq!(got, vec![ten, ten, ten, None]);
        assert_eq!(retry_after(ErrorCode::LowDisk, 9, young, max), None);
        assert_eq!(retry_after(ErrorCode::LowDisk, 1, Duration::minutes(121), max), None, "an old job waited anyway");
        assert_eq!(retry_after(ErrorCode::LowDisk, 1, Duration::hours(5), max), None);
        for c in ErrorCode::ALL.into_iter().filter(|c| !c.is_retryable()) {
            assert_eq!(retry_after(c, 1, Duration::zero(), max), None, "{c}");
        }
    }

    #[test]
    fn the_exhausted_hint_names_the_attempts_and_the_two_ways_on() {
        assert_eq!(
            exhausted_hint_el(5),
            "Δεν τα κατάφερε μετά από 5 προσπάθειες. Πατήστε «Δοκιμή ξανά» ή επικολλήστε άλλον σύνδεσμο."
        );
        assert_eq!(
            exhausted_hint_el(2),
            "Δεν τα κατάφερε μετά από 2 προσπάθειες. Πατήστε «Δοκιμή ξανά» ή επικολλήστε άλλον σύνδεσμο."
        );
        // One run and no retry: the age limit stopped it. No «1 προσπάθειες».
        let once = "Δεν τα κατάφερε και δεν ξαναδοκιμάστηκε αυτόματα, γιατί η εργασία είναι παλιά. Πατήστε «Δοκιμή ξανά» ή επικολλήστε άλλον σύνδεσμο.";
        assert_eq!(exhausted_hint_el(1), once);
        assert_eq!(exhausted_hint_el(0), once);
        for n in [0, 1, 2, 5] {
            assert!(!exhausted_hint_el(n).contains("Γίνεται αυτόματη επανάληψη"));
            assert!(!exhausted_hint_el(n).contains("1 προσπάθειες"));
        }
    }

    /// Only the codes whose own hint says a retry is under way swap it for the
    /// exhausted text; the share, a 403 and a full disk keep their own advice.
    #[test]
    fn only_the_passing_conditions_use_the_exhausted_hint() {
        let uses: Vec<ErrorCode> = ErrorCode::ALL.into_iter().filter(|c| c.uses_exhausted_hint()).collect();
        assert_eq!(
            uses,
            vec![
                ErrorCode::LiveStream,
                ErrorCode::Network,
                ErrorCode::ExtractTimeout,
                ErrorCode::DownloadTimeout,
                ErrorCode::DeliverTimeout,
                ErrorCode::RateLimited
            ]
        );
        for c in [ErrorCode::DeliveryFailed, ErrorCode::Http403, ErrorCode::LowDisk] {
            assert!(c.is_retryable() && !c.uses_exhausted_hint(), "{c}");
        }
        assert!(uses.iter().all(|c| c.is_retryable()));
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
            ErrorCode::NonPublicAddress,
            ErrorCode::DeliveryUnconfirmed,
            ErrorCode::RateLimited,
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
        assert_eq!(ErrorCode::ALL.len(), 34);
        let unique: std::collections::HashSet<_> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(unique.len(), 34);
    }

    /// P11.2b: the code of a delivery whose result nobody knows. Appended last
    /// (the list is append-only), stable, never retried on its own, set
    /// directly (never read out of a failure's text), and its hint tells MCR
    /// to look in Dalet before choosing between the two buttons.
    #[test]
    fn delivery_unconfirmed_is_the_last_code_terminal_and_says_check_dalet_first() {
        let c = ErrorCode::DeliveryUnconfirmed;
        // Appended last in P11.2b; only RATE_LIMITED (P11.4) came after it.
        assert_eq!(ErrorCode::ALL[32], c);
        assert_eq!(c.as_str(), "DELIVERY_UNCONFIRMED");
        assert_eq!(ErrorCode::from_code("DELIVERY_UNCONFIRMED"), Some(c));
        assert!(!c.is_retryable(), "a delivery that may be on air must not be retried on its own");
        assert!(!c.should_try_sniffer());
        assert!(!CLASSIFIED.contains(&c));
        assert_eq!(NOT_CLASSIFIED.iter().filter(|(x, _)| *x == c).count(), 1);
        assert_eq!(
            c.hint_el(),
            "Η παράδοση διακόπηκε και δεν είναι βέβαιο αν το αρχείο έφτασε. Ελέγξτε πρώτα στο Dalet αν υπάρχει ήδη: αν υπάρχει, πατήστε «Το έβαλα στο Dalet»· αν όχι, πατήστε «Δοκιμή ξανά»."
        );
        assert_ne!(c.hint_el(), ErrorCode::DeliveryFailed.hint_el());
        // The ordinary wait keeps its code, and stays retryable.
        assert_eq!(ErrorCode::DeliveryFailed.as_str(), "DELIVERY_FAILED");
        assert!(ErrorCode::DeliveryFailed.is_retryable());
        assert_eq!(code_in_text("x: DELIVERY_UNCONFIRMED: y"), None, "read out of text");
    }

    /// P11.4: the code of a platform refusing us for now. Appended last, stable,
    /// read back out of a failure's chain, retried after 2, 5, 10 and 15 minutes
    /// within the age limit (then the exhausted hint), never sniffed, and
    /// never held against a site's health.
    #[test]
    fn rate_limited_is_the_last_code_retried_four_times_and_never_sniffed() {
        let c = ErrorCode::RateLimited;
        assert_eq!(ErrorCode::ALL.last(), Some(&c));
        assert_eq!(c.as_str(), "RATE_LIMITED");
        assert_eq!(ErrorCode::from_code("RATE_LIMITED"), Some(c));
        assert_eq!(serde_json::to_string(&c).unwrap(), "\"RATE_LIMITED\"");
        assert_eq!(serde_json::from_str::<ErrorCode>("\"RATE_LIMITED\"").unwrap(), c);
        assert!(CLASSIFIED.contains(&c));
        assert_eq!(code_in_text("RATE_LIMITED: RATE_LIMITED: ERROR: [youtube] abc: Sign in to confirm"), Some(c));

        let max = Duration::minutes(120);
        let young = Duration::minutes(1);
        let got: Vec<_> = (1..=5).map(|n| retry_after(c, n, young, max)).collect();
        assert_eq!(
            got,
            vec![Some(Duration::minutes(2)), Some(Duration::minutes(5)), Some(Duration::minutes(10)), Some(Duration::minutes(15)), None]
        );
        assert_eq!(c.retry_max_attempts(), Some(5));
        assert_eq!(retry_after(c, 1, Duration::minutes(121), max), None, "the age limit holds for it too");
        assert!(c.uses_exhausted_hint());
        assert_eq!(c.hint_el(), "Ο ιστότοπος περιορίζει προσωρινά τις λήψεις. Γίνεται αυτόματη επανάληψη.");

        assert!(!c.should_try_sniffer());
        for url in ["https://www.youtube.com/watch?v=abc", "https://x.com/a/status/1", "https://www.news.example/story/1"] {
            assert!(!c.should_sniff(url), "{url}");
        }
        assert!(!omni_core::site_health::counts_against_site(c.as_str()));
        assert!(!crate::downloader::is_dead_candidate(c));
    }

    /// Review round 1: a rate limit that outlasted its retries on a platform
    /// ends with the site's cookie advice after the exhausted text; on a news
    /// page, and for any other code, the exhausted text alone.
    #[test]
    fn a_stopped_rate_limit_on_a_platform_adds_the_sites_cookie_advice() {
        let dir = tempfile::tempdir().unwrap();
        let repo = omni_core::repository::Repository::new(dir.path().join("omni.db")).unwrap();
        let x = "https://x.com/someone/status/1";
        assert_eq!(
            stopped_hint_el(ErrorCode::RateLimited, 5, 0, x, &repo),
            format!("{} {}", exhausted_hint_el(5), login_hint_el("x.com", false))
        );
        let yt = "https://www.youtube.com/watch?v=abcdefghijk";
        let got = stopped_hint_el(ErrorCode::RateLimited, 5, 0, yt, &repo);
        assert!(got.starts_with(&exhausted_hint_el(5)) && got.ends_with(&hint_for_job(ErrorCode::LoginRequired, yt, &repo)), "{got}");
        assert!(got.contains("cookies"), "{got}");
        // With a jar: the expired-jar wording, as a login wall gets it.
        let jar = omni_core::cookies::parse_netscape("x.com", ".x.com\tTRUE\t/\tTRUE\t0\tn\tvalue-123456\n").unwrap();
        repo.put_cookie_jar("x.com", &jar, "admin", chrono::Utc::now()).unwrap();
        assert!(stopped_hint_el(ErrorCode::RateLimited, 3, 0, x, &repo).ends_with(&login_hint_el("x.com", true)));
        // A news page, and the other passing codes: the exhausted text only.
        let article = "https://www.news.example/story/1";
        assert_eq!(stopped_hint_el(ErrorCode::RateLimited, 5, 0, article, &repo), exhausted_hint_el(5));
        assert_eq!(stopped_hint_el(ErrorCode::Network, 5, 0, x, &repo), exhausted_hint_el(5));
        assert_eq!(stopped_hint_el(ErrorCode::DownloadTimeout, 1, 0, yt, &repo), exhausted_hint_el(1));
    }

    /// P11.7: a stream still live is checked again 10, 30, 60 and 120 minutes
    /// after the job was queued (each run taking no time), and the failure of
    /// the fourth check goes to review with its own text. The 120-minute check
    /// is scheduled at the 60-minute mark, inside the 2-hour limit.
    #[test]
    fn a_live_stream_is_checked_at_10_30_60_and_120_minutes_then_reviewed() {
        let c = ErrorCode::LiveStream;
        let max = Duration::minutes(120);
        let created = chrono::DateTime::parse_from_rfc3339("2026-11-18T09:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let mut now = created;
        let mut checks = Vec::new();
        for attempts in 1..=4 {
            let wait = retry_after(c, attempts, now - created, max).unwrap_or_else(|| panic!("failure {attempts} not retried"));
            now += wait;
            checks.push((now - created).num_minutes());
        }
        assert_eq!(checks, vec![10, 30, 60, 120]);
        assert_eq!(retry_after(c, 5, now - created, max), None, "a fifth check");
        assert_eq!(c.retry_max_attempts(), Some(5), "the first run and four checks");
        // The last check runs late (a busy queue): it is still scheduled.
        assert_eq!(retry_after(c, 4, Duration::minutes(75), max), Some(Duration::minutes(60)));
        assert_eq!(retry_after(c, 1, Duration::minutes(121), max), None, "the age limit holds for it too");
        assert!(c.is_retryable() && c.uses_exhausted_hint());
        assert!(!c.should_sniff("https://www.youtube.com/watch?v=abc") && !c.should_sniff("https://www.news.example/story/1"));

        let dir = tempfile::tempdir().unwrap();
        let repo = omni_core::repository::Repository::new(dir.path().join("omni.db")).unwrap();
        let yt = "https://www.youtube.com/watch?v=abcdefghijk";
        // A job retried twice with `code` by the policy, as `retry_by_policy` re-queues it.
        let retried = |n: u32, code: ErrorCode| -> i64 {
            let id = repo
                .enqueue(&omni_core::models::NewJob::new(&format!("{yt}{n}"), &format!("{n}_ANNA_TEST"), "ANNA"), 24)
                .unwrap()
                .job_id();
            for _ in 0..2 {
                assert!(repo.lease_job_by_id(id, "w", 180).unwrap());
                let max = code.retry_max_attempts().unwrap();
                assert!(repo.requeue_retry(id, "w", Duration::seconds(0), code.as_str(), "x", max).unwrap());
            }
            id
        };
        let live = retried(1, c);
        assert_eq!(stopped_hint_el(c, 3, live, yt, &repo), LIVE_STREAM_EXHAUSTED_EL);
        // Worded without the age limit, which `retry.max_age_mins` sets (P11.7b).
        assert_eq!(
            LIVE_STREAM_EXHAUSTED_EL,
            "Η μετάδοση ήταν ακόμη ζωντανή (ή δεν είχε ξεκινήσει) σε όλους τους αυτόματους ελέγχους. Δοκιμάστε ξανά όταν ανέβει η εγγραφή, ή δώστε άλλον σύνδεσμο."
        );
        assert!(!LIVE_STREAM_EXHAUSTED_EL.chars().any(|ch| ch.is_ascii_digit()), "no number of hours");
        // Never checked again as live (already past the age limit): the exhausted text,
        // never the «wait» hint, since nothing will check it again (P11.7b2).
        assert_eq!(stopped_hint_el(c, 1, live + 100, yt, &repo), exhausted_hint_el(1));
        // P11.7b: retried for the network (the same max_attempts, 5, as a live row), then
        // found live past the age limit: it was never checked as live.
        let network = retried(2, ErrorCode::Network);
        assert_eq!(ErrorCode::Network.retry_max_attempts(), c.retry_max_attempts(), "max_attempts cannot tell them apart");
        assert_eq!(stopped_hint_el(c, 3, network, yt, &repo), exhausted_hint_el(3), "not re-queued as live");
        assert_ne!(exhausted_hint_el(3), c.hint_el());
        // The code omni-core's late rule names is this one.
        assert_eq!(c.as_str(), omni_core::models::LIVE_STREAM_CODE);
    }

    /// P11.4: what a rate limit looks like, and what still is not one.
    #[test]
    fn a_bot_check_or_a_429_is_rate_limited_and_a_real_wall_or_a_gone_video_is_not() {
        for msg in [
            "ERROR: [youtube] abc123: Sign in to confirm you’re not a bot. Use --cookies-from-browser or --cookies for the authentication. See https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp for how to manually pass your cookies",
            "ERROR: [youtube] abc123: Sign in to confirm you're not a bot",
            "HTTP Error 429: Too Many Requests",
            "ERROR: [instagram] abc: 429 Too Many Requests",
            "ERROR: [facebook] 1: This content isn't available right now, try again later",
        ] {
            assert_eq!(classify_download_error(msg), ErrorCode::RateLimited, "{msg}");
        }
        for (msg, want) in [
            ("ERROR: [youtube] 5: Sign in to confirm your age. This video may be inappropriate for some users.", ErrorCode::LoginRequired),
            ("ERROR: [youtube] 6: This video is age-restricted", ErrorCode::LoginRequired),
            ("ERROR: [youtube] 7: Join this channel to get access to members-only content", ErrorCode::LoginRequired),
            ("ERROR: [twitter] 8: NSFW tweet requires authentication. Use --cookies", ErrorCode::LoginRequired),
            ("ERROR: [Instagram] abc: Requested content is not available, rate-limit reached or login required. Use --cookies", ErrorCode::LoginRequired),
            ("ERROR: [youtube] abc: Private video. Sign in if you've been granted access", ErrorCode::PrivateOrRemoved),
            ("ERROR: [youtube] abc: Video unavailable. This video has been removed by the uploader", ErrorCode::PrivateOrRemoved),
            ("ERROR: [youtube] abc: The uploader has not made this video available in your country", ErrorCode::GeoBlocked),
            ("ERROR: [youtube] abc: This live event will begin in 3 hours", ErrorCode::LiveStream),
            ("ERROR: Unsupported URL: https://news.example/too-many-requests-story", ErrorCode::UnsupportedUrl),
            // Review round 1: the bot check outside YouTube is a login wall.
            ("ERROR: [twitter] 9: Sign in to confirm you're not a bot. Use --cookies", ErrorCode::LoginRequired),
            ("ERROR: [Instagram] abc: Sign in to confirm you’re not a bot. Use --cookies-from-browser", ErrorCode::LoginRequired),
            ("ERROR: Sign in to confirm you're not a bot. Use --cookies", ErrorCode::LoginRequired),
            ("ERROR: [youtube:tab] UCabc: Sign in to confirm you’re not a bot. Use --cookies", ErrorCode::RateLimited),
            // The explicit limits stay RATE_LIMITED on every platform.
            ("ERROR: [twitter] 9: Rate limit exceeded", ErrorCode::RateLimited),
            ("ERROR: [Instagram] abc: HTTP Error 429: Too Many Requests", ErrorCode::RateLimited),
        ] {
            assert_eq!(classify_download_error(msg), want, "{msg}");
        }
    }

    #[test]
    fn the_approval_codes_are_terminal_stable_and_greek() {
        let cases = [
            (ErrorCode::RejectedByMcr, "REJECTED_BY_MCR"),
            (ErrorCode::ApprovalExpired, "APPROVAL_EXPIRED"),
            (ErrorCode::HeldFileChanged, "HELD_FILE_CHANGED"),
            (ErrorCode::RepairExhausted, "REPAIR_EXHAUSTED"),
            (ErrorCode::NonPublicAddress, "NON_PUBLIC_ADDRESS"),
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

    /// P11.1: a code added later and forgotten in both lists is caught here.
    #[test]
    fn every_code_is_either_classified_or_says_why_not() {
        for c in ErrorCode::ALL {
            let classified = CLASSIFIED.iter().filter(|x| **x == c).count();
            let not = NOT_CLASSIFIED.iter().filter(|(x, _)| *x == c).count();
            assert_eq!(classified + not, 1, "{c}: in CLASSIFIED {classified}x, in NOT_CLASSIFIED {not}x");
        }
        for (c, why) in NOT_CLASSIFIED {
            assert!(!why.trim().is_empty(), "{c} needs a reason");
        }
        assert_eq!(CLASSIFIED.len() + NOT_CLASSIFIED.len(), ErrorCode::ALL.len());
    }

    /// Every classified code comes back out of a chain shaped like the
    /// worker's `{e:#}`, at the front or in the middle.
    #[test]
    fn every_classified_code_is_found_in_a_real_shaped_chain() {
        for c in CLASSIFIED {
            let code = c.as_str();
            for text in [
                format!("{code}: outer: middle: inner detail"),
                format!("bmxtranswrap RDD9 failed: {code}: bmxtranswrap exited with code Some(1). stderr tail: boom"),
                format!("outer: middle (caused by {code})"),
                code.to_string(),
            ] {
                assert_eq!(code_in_text(&text), Some(*c), "{text}");
            }
        }
        assert_eq!(code_in_text("ffmpeg exited with code Some(1). stderr tail: boom"), None);
        assert_eq!(code_in_text(""), None);
    }

    /// The old `contains` read a code inside a longer identifier.
    #[test]
    fn a_code_inside_a_longer_identifier_is_not_that_code() {
        for text in ["MY_NETWORK_X", "HTTP_4031", "XLOW_DISK", "NETWORKS", "a file 3_NETWORK_OUTAGE.mp4", "2HTTP_403"] {
            assert_eq!(code_in_text(text), None, "{text}");
        }
        for text in ["… (NETWORK)", "NETWORK: reset", "x:NETWORK", "networkNETWORK.", "Δίκτυο NETWORK"] {
            assert_eq!(code_in_text(text), Some(ErrorCode::Network), "{text}");
        }
        // Every occurrence is looked at, not only the first.
        assert_eq!(code_in_text("MY_NETWORK_X then NETWORK: reset"), Some(ErrorCode::Network));
        assert_eq!(code_in_text("XLOW_DISK: REWRAP_FAILED"), Some(ErrorCode::RewrapFailed));
    }

    #[test]
    fn the_more_exact_code_wins_whatever_its_place_in_the_text() {
        let both = "NETWORK: x: NO_STREAM_FOUND: no video";
        assert_eq!(code_in_text(both), Some(ErrorCode::NoStreamFound));
        assert_eq!(code_in_text("REWRAP_FAILED: LOW_DISK: bmxtranswrap exited"), Some(ErrorCode::LowDisk));
        assert_eq!(code_in_text("LOW_DISK: REWRAP_FAILED: bmxtranswrap exited"), Some(ErrorCode::LowDisk));
        assert_eq!(code_in_text("TRANSCODE_FAILED: PROBE_FAILED for job #3"), Some(ErrorCode::ProbeFailed));
        // The original 18 keep their order.
        assert_eq!(&CLASSIFIED[..18], &[
            ErrorCode::ManualDownload,
            ErrorCode::HeldFileChanged,
            ErrorCode::ComplianceFailed,
            ErrorCode::ProbeFailed,
            ErrorCode::LoginRequired,
            ErrorCode::GeoBlocked,
            ErrorCode::PrivateOrRemoved,
            ErrorCode::PageNotFound,
            ErrorCode::LiveStream,
            ErrorCode::Http403,
            ErrorCode::UnsupportedUrl,
            ErrorCode::NoStreamFound,
            ErrorCode::DownloadTimeout,
            ErrorCode::TranscodeTimeout,
            ErrorCode::RewrapTimeout,
            ErrorCode::DeliveryFailed,
            ErrorCode::LowDisk,
            ErrorCode::Network,
        ]);
    }

    /// Job 68: bmxtranswrap on a full disk read as "unexpected error".
    #[test]
    fn a_full_disk_is_recognised_in_each_tool_s_words() {
        let job68 = "bmxtranswrap exited with code Some(1). stderr tail: ERROR: 'mxf_file->write(data, size) == size' check failed\nERROR: near bmx/src/rdd9_mxf/RDD9ContentPackage.cpp:187\nERROR: BMX exception caught\n";
        assert!(disk_full_text(job68));
        assert!(disk_full_text("[out#0/mxf @ 000001] av_interleaved_write_frame(): No space left on device"));
        assert!(disk_full_text("Failed writing the file (os error 112)"));
        assert!(disk_full_text("There is not enough space on the disk. (os error 112)"));
        assert!(!disk_full_text("ERROR: Failed to open file 'C:\\temp\\7_transcode.mxf'\nERROR: BMX exception caught"));
        assert!(!disk_full_text("[mpeg2video @ 000001] VBV buffer too small for bitrate\nConversion failed!"));
        assert!(!disk_full_text("The system cannot find the file specified. (os error 1123)"));
    }
}
