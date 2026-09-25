//! Mail source abstraction (plan P4.1).
//!
//! The watcher only talks to [`MailSource`]. Graph (plan P4.2) is the primary
//! implementation; IMAP stays for on-prem servers and tests. Both reduce a
//! message to [`InboundMail`], so the parser never knows where mail came from.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::mail::InboundMail;

/// What happened to a message, as told back to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MailOutcome {
    /// Jobs (if any) are persisted: mark read, file under "processed".
    Processed,
    /// Could not be processed after retries: leave it **unread** where a
    /// human will see it (defect E-02 lost these silently).
    Failed,
}

impl MailOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Processed => "PROCESSED",
            Self::Failed => "FAILED",
        }
    }
}

/// Mailbox reachability, for the admin Mail card.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailHealth {
    pub ok: bool,
    /// One operator-readable line. Never carries a credential.
    pub detail: String,
}

#[async_trait]
pub trait MailSource: Send + Sync {
    /// Short description for logs and the status bar, e.g. `imap mail.example.gr`.
    fn describe(&self) -> String;

    /// Credentials and addresses are present. An unconfigured source is
    /// reported as disabled, not as an outage.
    fn is_configured(&self) -> bool;

    /// Up to `limit` messages not yet marked processed, oldest first.
    /// Fetching must not itself mark anything read.
    async fn fetch_unprocessed(&self, limit: usize) -> Result<Vec<InboundMail>>;

    /// Record the outcome on the server. Called only after everything the
    /// message produced has been persisted.
    async fn mark_processed(&self, mail_id: &str, outcome: MailOutcome) -> Result<()>;

    /// Save one attachment to `dest` (a file path) and return it.
    async fn download_attachment(&self, mail_id: &str, attachment_id: &str, dest: &Path) -> Result<PathBuf>;

    /// Reply to the sender from the ingest mailbox (plan P5.2).
    async fn reply(&self, mail_id: &str, html: &str, text: &str) -> Result<()>;

    async fn health(&self) -> MailHealth;
}
