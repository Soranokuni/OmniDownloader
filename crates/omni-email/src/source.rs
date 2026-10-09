//! Mail source abstraction (plan P4.1).
//!
//! The watcher only talks to [`MailSource`]. Graph (plan P4.2) is the primary
//! implementation (IMAP was removed in P4.7); tests use scripted doubles.
//! Every source reduces a message to [`InboundMail`], so the parser never
//! knows where mail came from.
//!
//! A poll is two steps (plan P4.8): a cheap [`MailSource::list_changed`] of
//! headers, then [`MailSource::fetch_mail`] only for the messages the
//! database has not seen. The mailbox may be read-only, so nothing here may
//! rely on a message being marked.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
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

/// One entry of a mailbox listing: enough to decide whether to fetch it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailHeader {
    /// Provider id, for [`MailSource::fetch_mail`].
    pub id: String,
    /// RFC 5322 `Message-ID`, with its angle brackets; may be empty.
    pub internet_message_id: String,
    pub subject: String,
    pub from_address: String,
    pub received_at: Option<DateTime<Utc>>,
    /// When the server last changed the message: arrival, a move into the
    /// inbox (a mail rescued from Junk keeps its old `received_at`), a flag.
    pub modified_at: DateTime<Utc>,
}

/// The message is no longer there (deleted or moved away between the
/// listing and the fetch). Nothing to process and nothing to retry.
#[derive(Debug, Clone)]
pub struct MailGone(pub String);

impl std::fmt::Display for MailGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "message {} is no longer in the mailbox", self.0)
    }
}

impl std::error::Error for MailGone {}

/// Mailbox reachability, for the admin Mail card.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailHealth {
    pub ok: bool,
    /// One operator-readable line. Never carries a credential.
    pub detail: String,
}

#[async_trait]
pub trait MailSource: Send + Sync {
    /// Short description for logs and the status bar, e.g. `graph ingest@example.gr`.
    fn describe(&self) -> String;

    /// Stable name of the mailbox, for its checkpoint row, e.g.
    /// `graph:ingest@example.gr`. Must not change between runs.
    fn checkpoint_key(&self) -> String;

    /// Credentials and addresses are present. An unconfigured source is
    /// reported as disabled, not as an outage.
    fn is_configured(&self) -> bool;

    /// Inbox messages changed at or after `since`, oldest change first, at
    /// most `max`. Read-only: listing marks nothing.
    async fn list_changed(&self, since: DateTime<Utc>, max: usize) -> Result<Vec<MailHeader>>;

    /// The full message, attachment metadata included. A message that has
    /// vanished since the listing is a [`MailGone`] error.
    async fn fetch_mail(&self, id: &str) -> Result<InboundMail>;

    /// Record the outcome on the server. Called only after everything the
    /// message produced has been persisted. A read-only source does nothing:
    /// the database, not the mailbox, is what records a message as handled.
    async fn mark_processed(&self, mail_id: &str, outcome: MailOutcome) -> Result<()>;

    /// Save one attachment to `dest` (a file path) and return it.
    async fn download_attachment(&self, mail_id: &str, attachment_id: &str, dest: &Path) -> Result<PathBuf>;

    /// Reply to the sender from the ingest mailbox (plan P5.2).
    async fn reply(&self, mail_id: &str, html: &str, text: &str) -> Result<()>;

    async fn health(&self) -> MailHealth;
}
