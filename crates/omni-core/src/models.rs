use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobStatus {
    Pending,
    Extracting,
    Downloading,
    Transcoding,
    Rewrapping,
    Completed,
    Failed,
    RequiresReview,
    ManualDownload,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Extracting => "EXTRACTING",
            Self::Downloading => "DOWNLOADING",
            Self::Transcoding => "TRANSCODING",
            Self::Rewrapping => "REWRAPPING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::RequiresReview => "REQUIRES_REVIEW",
            Self::ManualDownload => "MANUAL_DOWNLOAD",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_uppercase().as_str() {
            "PENDING" => Self::Pending,
            "EXTRACTING" => Self::Extracting,
            "DOWNLOADING" => Self::Downloading,
            "TRANSCODING" => Self::Transcoding,
            "REWRAPPING" => Self::Rewrapping,
            "COMPLETED" => Self::Completed,
            "FAILED" => Self::Failed,
            "MANUAL_DOWNLOAD" => Self::ManualDownload,
            _ => Self::RequiresReview,
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
    pub created_at: Option<DateTime<Utc>>,
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
