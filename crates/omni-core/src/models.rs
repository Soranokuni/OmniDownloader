use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Coarse job state (plan P1.1).
///
/// The fine-grained position in the pipeline lives in [`JobStage`]. Splitting
/// them means the queue can ask "is this job running?" with one predicate, and
/// adding a stage never requires touching status handling or the UI's status
/// filters.
///
/// `as_str` values are a stable API: they appear in the REST responses, the SSE
/// stream and the panels' filters. Keep the existing spellings working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobStatus {
    /// Waiting for a worker. Also where retries land.
    Pending,
    /// A worker holds a live lease on it. The stage says where it is.
    Running,
    Completed,
    /// Terminal failure; the error_code says why and whether a retry would help.
    Failed,
    /// Needs a human in MCR. Never delivered.
    RequiresReview,
    /// A file-locker link a human must fetch (WeTransfer and friends).
    ManualDownload,
    /// Cancelled by an operator.
    Cancelled,
    /// The operator dropped the file into Dalet themselves.
    CompletedManual,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::RequiresReview => "REQUIRES_REVIEW",
            Self::ManualDownload => "MANUAL_DOWNLOAD",
            Self::Cancelled => "CANCELLED",
            Self::CompletedManual => "COMPLETED_MANUAL",
        }
    }

    /// Parse a stored status.
    ///
    /// Returns `None` for anything unrecognised rather than silently mapping it
    /// to RequiresReview, which hid database corruption behind a plausible
    /// state (defect D-21). Callers decide what to do with an unknown value;
    /// the repository logs it.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_uppercase().as_str() {
            "PENDING" => Self::Pending,
            "RUNNING" => Self::Running,
            "COMPLETED" => Self::Completed,
            "FAILED" => Self::Failed,
            "REQUIRES_REVIEW" => Self::RequiresReview,
            "MANUAL_DOWNLOAD" => Self::ManualDownload,
            "CANCELLED" => Self::Cancelled,
            "COMPLETED_MANUAL" => Self::CompletedManual,
            // Legacy states from before the state machine; migration 2 rewrites
            // the rows, but a downgrade or an external tool could still write one.
            "EXTRACTING" | "DOWNLOADING" | "TRANSCODING" | "REWRAPPING" => Self::Running,
            _ => return None,
        })
    }

    /// Nothing further will happen to this job without an operator acting.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::CompletedManual
                | Self::RequiresReview
                | Self::ManualDownload
        )
    }

    /// The job still occupies the queue: dedup must consider it.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// Position within the pipeline for a RUNNING job (plan P1.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobStage {
    Queued,
    Extract,
    Download,
    Probe,
    Transcode,
    Rewrap,
    Verify,
    Deliver,
    Archive,
    Done,
}

impl JobStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Extract => "EXTRACT",
            Self::Download => "DOWNLOAD",
            Self::Probe => "PROBE",
            Self::Transcode => "TRANSCODE",
            Self::Rewrap => "REWRAP",
            Self::Verify => "VERIFY",
            Self::Deliver => "DELIVER",
            Self::Archive => "ARCHIVE",
            Self::Done => "DONE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_uppercase().as_str() {
            "QUEUED" => Self::Queued,
            "EXTRACT" => Self::Extract,
            "DOWNLOAD" => Self::Download,
            "PROBE" => Self::Probe,
            "TRANSCODE" => Self::Transcode,
            "REWRAP" => Self::Rewrap,
            "VERIFY" => Self::Verify,
            "DELIVER" => Self::Deliver,
            "ARCHIVE" => Self::Archive,
            "DONE" => Self::Done,
            _ => return None,
        })
    }

    /// Greek label for the MCR panel's stage chip.
    pub fn label_el(&self) -> &'static str {
        match self {
            Self::Queued => "Σε αναμονή",
            Self::Extract => "Εντοπισμός",
            Self::Download => "Λήψη",
            Self::Probe => "Έλεγχος πηγής",
            Self::Transcode => "Μετατροπή",
            Self::Rewrap => "Ενθυλάκωση",
            Self::Verify => "Επαλήθευση",
            Self::Deliver => "Παράδοση",
            Self::Archive => "Αρχειοθέτηση",
            Self::Done => "Ολοκληρώθηκε",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub url: String,
    pub slug: String,
    pub journalist: String,
    pub keyword: String,
    pub index_str: String,
    pub status: JobStatus,
    /// Where in the pipeline a RUNNING job is; `Queued` otherwise.
    pub stage: JobStage,
    pub progress: f64,
    pub speed: String,
    pub eta: String,
    pub priority: i32,
    pub error_message: Option<String>,
    pub media_format: String,
    pub file_path: Option<String>,
    pub duration_secs: f64,
    pub submitted_by_user_id: Option<i64>,
    pub email_source: Option<String>,
    pub notes: Option<String>,

    /// Dedup key (`omni_core::urlnorm`), not what gets downloaded.
    pub url_normalized: Option<String>,
    /// Times this job has been leased. Compared against `max_attempts`.
    pub attempts: i32,
    pub max_attempts: i32,
    /// `"{hostname}:{pid}:{worker_n}"` while leased, else `None`.
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub stage_started_at: Option<DateTime<Utc>>,
    /// Retry backoff: `lease_job` will not pick the job up before this.
    pub not_before: Option<DateTime<Utc>>,
    /// Stable machine-readable failure reason; drives retries and MCR hints.
    pub error_code: Option<String>,
    /// Downloaded source, kept for the archive and for re-runs.
    pub source_path: Option<String>,
    /// Compliance report from the pre-delivery gate (plan P1.6).
    pub compliance_json: Option<String>,
    /// Alternative streams the sniffer found, for the MCR candidate picker.
    pub candidates_json: Option<String>,
    /// `direct` | `adapter:<name>` | `sniffer` | `attachment` | `locker`.
    pub extraction_method: Option<String>,
    /// `{"download_ms": .., "transcode_ms": ..}` for the benchmarks.
    pub stage_timings_json: Option<String>,
    pub email_message_id: Option<String>,
    /// The group the job was queued for (plan P4.18); a label only.
    #[serde(default)]
    pub group_code: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,

    /// Stored timestamp, `None` when the row predates real timestamps or the
    /// value is unreadable. Never substituted with "now" (defect D-11): the MCR
    /// archive exists to answer *when* something went to Dalet, and a fabricated
    /// value is worse than an honest blank.
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    #[serde(rename = "open_mcr")]
    OpenMcr,
    User,
}

impl UserRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::OpenMcr => "open_mcr",
            Self::User => "user",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "admin" => Self::Admin,
            "open_mcr" | "mcr" => Self::OpenMcr,
            _ => Self::User,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: UserRole,
    pub full_name: String,
    pub journalist_surname: Option<String>,
    pub is_active: bool,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journalist {
    pub id: i64,
    pub surname: String,
    pub full_name: String,
    pub emails: Vec<String>,
    pub default_priority: i32,
    /// Other spellings the email parser accepts for this journalist (plan
    /// P4.3): Greek surname, genitive, first name — `["ΠΑΠΑΔΑΚΗ", "ΑΝΝΑΣ"]`.
    /// Compared accent- and case-insensitively, in ELOT 743 Latin.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Group codes this person belongs to (plan P4.17); the first is their
    /// default group.
    #[serde(default)]
    pub groups: Vec<String>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Queue depth, for the status panel (plan P6.2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueSummary {
    pub pending: i64,
    /// Anything currently being worked on, whatever stage it is in.
    pub running: i64,
    pub review: i64,
    pub manual: i64,
    pub completed: i64,
    pub failed: i64,
    pub total: i64,
    /// How long the oldest waiting job has been waiting. `None` when nothing is
    /// pending. This, not the pending count, is what says "the pipeline has
    /// stalled": twenty pending jobs are normal right after a rundown arrives
    /// and alarming an hour later.
    pub oldest_pending_age_secs: Option<i64>,
}

/// One row of `login_attempts` (plan P2.2).
///
/// Records the *attempt*, never the credential: the password is not a field
/// here and must never become one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginAttempt {
    pub id: i64,
    pub at: Option<DateTime<Utc>>,
    pub email: Option<String>,
    pub ip: Option<String>,
    pub successful: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLog {
    pub id: i64,
    pub level: String,
    pub category: String,
    pub message: String,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MezzanineSpecs {
    pub video_codec: String,
    pub container: String,
    pub resolution: String,
    pub frame_rate: String,
    pub scan_type: String,
    pub video_bitrate: String,
    pub chroma_subsampling: String,
    pub gop_structure: String,
    pub audio_codec: String,
    pub audio_channels: String,
    pub audio_sample_rate: String,
    pub audio_bit_depth: String,
    pub audio_matrix: String,
}

impl Default for MezzanineSpecs {
    fn default() -> Self {
        Self {
            video_codec: "MPEG-2 (Sony XDCAM HD422)".into(),
            container: "SMPTE RDD9 OP1a MXF".into(),
            resolution: "1920x1080".into(),
            frame_rate: "25 fps (PAL)".into(),
            scan_type: "Interlaced (Top Field First)".into(),
            video_bitrate: "50 Mbps (CBR)".into(),
            chroma_subsampling: "4:2:2 (8-bit)".into(),
            gop_structure: "M=3, N=12 (Long GOP)".into(),
            audio_codec: "Uncompressed PCM (pcm_s24le)".into(),
            audio_channels: "8 Discrete Mono Channels (EBU R48)".into(),
            audio_sample_rate: "48,000 Hz".into(),
            audio_bit_depth: "24-bit".into(),
            audio_matrix: "Ch1: Left, Ch2: Right, Ch3-8: Silence".into(),
        }
    }
}

/// One line of a job's timeline (plan P1.1).
///
/// The MCR job drawer renders these so an operator can see what the pipeline
/// did and where it stopped, without reading the daemon log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEvent {
    pub id: i64,
    pub job_id: i64,
    pub at: Option<DateTime<Utc>>,
    /// Pipeline stage this happened in, if it belonged to one.
    pub stage: Option<String>,
    /// INFO | WARN | ERROR.
    pub level: String,
    pub message: String,
}

/// Outcome of [`crate::repository::Repository::enqueue`] (plan P1.1).
///
/// Deduplication is a policy decision, not a database error. The old schema had
/// `UNIQUE(url)`, so re-queuing a link failed with a constraint violation that
/// the email watcher could only report as "something went wrong" (defect D-10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Enqueued {
    /// A new job was created.
    Created { id: i64 },
    /// The same normalized URL is already queued or running. Adding it again
    /// would put two identical files in the watchfolder.
    DuplicateActive { existing_id: i64 },
    /// The same normalized URL completed recently for the same journalist.
    /// Usually a re-sent email; the reply tells the journalist it is already
    /// delivered rather than silently doing nothing.
    DuplicateRecent { existing_id: i64 },
}

impl Enqueued {
    /// The job id involved, whether newly created or the existing duplicate.
    pub fn job_id(&self) -> i64 {
        match self {
            Self::Created { id } => *id,
            Self::DuplicateActive { existing_id } | Self::DuplicateRecent { existing_id } => {
                *existing_id
            }
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, Self::Created { .. })
    }
}

/// One row of `processed_mail` (plan P4.2, defect E-07).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessedMail {
    /// RFC 5322 Message-ID, or `source:{provider id}` when a message has none.
    pub internet_message_id: String,
    /// Provider id at the time (Graph message id, IMAP UID).
    pub source_id: Option<String>,
    pub processed_at: Option<DateTime<Utc>>,
    /// Parser outcome (`JOBS`, `PHOTOS_ONLY`, `NO_LINKS`) or `FAILED`.
    pub outcome: String,
    pub from_address: Option<String>,
    pub subject: Option<String>,
    /// What the message produced, as the email crate records it.
    pub jobs_json: String,
}

/// A job to be queued (plan P1.1).
#[derive(Debug, Clone)]
pub struct NewJob {
    pub url: String,
    pub slug: String,
    pub journalist: String,
    pub keyword: String,
    pub index_str: String,
    pub priority: i32,
    pub status: JobStatus,
    pub submitted_by_user_id: Option<i64>,
    pub notes: Option<String>,
    pub email_source: Option<String>,
    pub email_message_id: Option<String>,
    pub extraction_method: Option<String>,
    /// Group label (plan P4.18).
    pub group_code: Option<String>,
}

impl NewJob {
    /// A minimal job with newsroom defaults, for the web form and tests.
    pub fn new(url: impl Into<String>, slug: impl Into<String>, journalist: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            slug: slug.into(),
            journalist: journalist.into(),
            keyword: "ASSET".into(),
            index_str: "1".into(),
            priority: 0,
            status: JobStatus::Pending,
            submitted_by_user_id: None,
            notes: None,
            email_source: None,
            email_message_id: None,
            extraction_method: None,
            group_code: None,
        }
    }
}
