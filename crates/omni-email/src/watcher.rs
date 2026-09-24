use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};

use omni_core::config::AppConfig;
use omni_core::health::{Check, HealthState};
use omni_core::models::{Enqueued, JobStatus, NewJob, ProcessedMail};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

use crate::imap_source::ImapMailSource;
use crate::mail::InboundMail;
use crate::parser::{self, ParsedEmail, Tier};
use crate::source::{MailOutcome, MailSource};

/// Messages fetched per poll. The rest wait for the next poll, oldest first.
const FETCH_LIMIT: usize = 20;

/// Consecutive processing failures before a message is given up on and left
/// unread for a human (plan P4.2: "Omni/Failed"). A transient failure (the
/// database briefly locked, the disk full) gets this many polls to clear.
pub const MAX_PROCESS_ATTEMPTS: u32 = 3;

#[derive(Clone)]
pub struct EmailWatcher {
    config: AppConfig,
    repo: Repository,
    source: Arc<dyn MailSource>,
    /// Where the poll result is reported (plan P6.2). `None` in tests and in
    /// any caller that does not care.
    health: Option<HealthState>,
    /// Failed processing attempts per message id, since start-up.
    attempts: Arc<Mutex<HashMap<String, u32>>>,
}

/// What one poll did, for health and tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PollReport {
    pub fetched: usize,
    pub processed: usize,
    /// Failed this time, will be retried next poll.
    pub retrying: usize,
    /// Gave up: marked failed on the server.
    pub failed: usize,
}

/// Reduce an error chain to one line an operator can act on.
///
/// The full chain goes to the log and to `last_error`; this is what the MCR
/// panel shows, so it must be short, must name the *first* cause rather than
/// the outermost wrapper, and must never carry a credential — which is why it
/// takes the anyhow root and not the formatted `{:#}` chain.
fn short_reason(e: &anyhow::Error) -> String {
    let root = e.chain().last().map(|c| c.to_string()).unwrap_or_default();
    let text = if root.is_empty() { e.to_string() } else { root };
    let lower = text.to_ascii_lowercase();

    // The two that matter are worth naming plainly, because the remedy differs
    // and an operator should not have to read an IMAP error to tell them apart.
    if lower.contains("authenticationfailed")
        || lower.contains("login failed")
        || lower.contains("invalid credentials")
    {
        return "mailbox rejected the credentials".to_string();
    }
    if lower.contains("timed out") || lower.contains("connect") || lower.contains("dns") {
        return "cannot reach the mail server".to_string();
    }
    text.chars().take(120).collect()
}

impl EmailWatcher {
    pub fn new(config: AppConfig, repo: Repository) -> Self {
        let source = Arc::new(ImapMailSource::new(
            &config.imap_server,
            config.imap_port,
            &config.email_address,
            &config.email_password,
        ));
        Self::with_source(config, repo, source)
    }

    /// A watcher over any mail source (Graph, IMAP, or a test double).
    pub fn with_source(config: AppConfig, repo: Repository, source: Arc<dyn MailSource>) -> Self {
        Self {
            config,
            repo,
            source,
            health: None,
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Report poll outcomes into the shared health state.
    pub fn with_health(mut self, health: HealthState) -> Self {
        self.health = Some(health);
        self
    }

    fn report_health(&self, check: Check) {
        if let Some(h) = &self.health {
            // `set_if_changed` keeps `last_ok` meaning "when this last worked".
            // A 20-second poll rewriting an identical success would make the
            // timestamp advance during an outage.
            h.set_if_changed(omni_core::health::checks::MAIL, check);
        }
    }

    pub fn test_connection(server: &str, port: u16, email: &str, pass: &str) -> Result<()> {
        ImapMailSource::new(server, port, email, pass).test_connection_blocking()
    }

    pub async fn start_polling_loop(self: Arc<Self>, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
        info!("EmailWatcher: watching {}", self.source.describe());

        let poll_interval = Duration::from_secs(self.config.email_poll_interval_secs.max(10));

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("EmailWatcher: Received shutdown signal. Exiting watchdog loop.");
                    break;
                }
                _ = tokio::time::sleep(poll_interval) => {
                    if !self.source.is_configured() {
                        warn!("EmailWatcher: Credentials not configured. Sleeping...");
                        self.report_health(Check::disabled("Mailbox"));
                        continue;
                    }

                    match self.poll_once().await {
                        Err(e) => {
                            error!("EmailWatcher error: {:#}", e);
                            let _ = self.repo.log_audit("ERROR", "EMAIL", &format!("Mail poll error: {}", short_reason(&e)));
                            // The panel used to say "Mail: Active" through
                            // exactly this (defect W-09). A short reason, not
                            // the whole chain: this string is shown to an
                            // operator, and it must not carry a credential.
                            self.report_health(
                                Check::degraded(short_reason(&e)).with_error(format!("{e:#}")),
                            );
                        }
                        Ok(_) => {
                            self.report_health(Check::ok(format!(
                                "polling {} every {}s",
                                self.source.describe(),
                                poll_interval.as_secs()
                            )));
                        }
                    }
                }
            }
        }
    }

    /// One poll: fetch, process, and mark each message **only after** what it
    /// produced is persisted (defect E-02 marked everything `\Seen`, success
    /// or not, so a failed parse silently lost the links).
    pub async fn poll_once(&self) -> Result<PollReport> {
        let mails = self.source.fetch_unprocessed(FETCH_LIMIT).await?;
        let mut report = PollReport {
            fetched: mails.len(),
            ..Default::default()
        };
        if !mails.is_empty() {
            info!("EmailWatcher: {} unread message(s)", mails.len());
        }

        for mail in &mails {
            match self.process_mail(mail).await {
                Ok(Handled::GaveUpEarlier) => {
                    // Recorded as failed on an earlier run and still showing
                    // as unprocessed: leave it for the human it was left for.
                }
                Ok(Handled::Done) => {
                    self.attempts.lock().unwrap().remove(&mail.id);
                    if let Err(e) = self.source.mark_processed(&mail.id, MailOutcome::Processed).await {
                        // The jobs exist; the next poll sees the message again
                        // and dedup keeps it from queueing twice.
                        warn!("EmailWatcher: could not mark message {} processed: {e:#}", mail.id);
                    }
                    report.processed += 1;
                }
                Err(e) => {
                    let n = {
                        let mut attempts = self.attempts.lock().unwrap();
                        let n = attempts.entry(mail.id.clone()).or_insert(0);
                        *n += 1;
                        *n
                    };
                    error!(
                        "EmailWatcher: processing '{}' failed (attempt {n}/{MAX_PROCESS_ATTEMPTS}): {e:#}",
                        mail.subject
                    );
                    if n >= MAX_PROCESS_ATTEMPTS {
                        let _ = self.repo.log_audit(
                            "ERROR",
                            "EMAIL",
                            &format!("Gave up on email '{}' from {}: {}", mail.subject, mail.from_address, short_reason(&e)),
                        );
                        let _ = self.repo.record_processed_mail(&ProcessedMail {
                            internet_message_id: mail_key(mail),
                            source_id: Some(mail.id.clone()),
                            processed_at: None,
                            outcome: MailOutcome::Failed.as_str().into(),
                            from_address: Some(mail.from_address.clone()),
                            subject: Some(mail.subject.clone()),
                            jobs_json: "[]".into(),
                        });
                        match self.source.mark_processed(&mail.id, MailOutcome::Failed).await {
                            Ok(()) => {
                                self.attempts.lock().unwrap().remove(&mail.id);
                                report.failed += 1;
                            }
                            Err(e) => warn!("EmailWatcher: could not mark message {} failed: {e:#}", mail.id),
                        }
                    } else {
                        report.retrying += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Parse one message and queue its jobs (plan P4.5).
    ///
    /// Returns only after every job is persisted; any error leaves the
    /// message for the next poll. A retry after a partial failure is safe: the
    /// jobs already created come back from `enqueue` as `DuplicateActive`.
    async fn process_mail(&self, mail: &InboundMail) -> Result<Handled> {
        let key = mail_key(mail);

        // E-07: the server can show a message as unread again (a lost flag,
        // a failed move, a journalist dragging it back). Seen before → done.
        if let Some(prev) = self.repo.get_processed_mail(&key)? {
            if prev.outcome == MailOutcome::Failed.as_str() {
                return Ok(Handled::GaveUpEarlier);
            }
            info!("EmailWatcher: {key} was already processed ({}); not queueing again", prev.outcome);
            return Ok(Handled::Done);
        }

        info!("EmailWatcher: processing email from '{}', subject '{}'", mail.from_address, mail.subject);

        let roster = self.repo.list_journalists()?;
        let parsed = parser::parse(mail, &roster, &self.config.parser);
        let default_priority = roster
            .iter()
            .find(|j| j.surname == parsed.journalist.surname)
            .map(|j| j.default_priority)
            .unwrap_or(0);
        let records = self.enqueue_parsed(mail, &parsed, default_priority)?;

        info!(
            "EmailWatcher: {} → {} ({:?}), {} job(s), {} new",
            key,
            parsed.journalist.surname,
            parsed.journalist.how,
            records.len(),
            records.iter().filter(|r| r.result.is_new()).count()
        );
        if !parsed.warnings.is_empty() {
            let codes: Vec<&str> = parsed.warnings.iter().map(|w| w.code.as_str()).collect();
            let _ = self.repo.log_audit(
                "WARN",
                "EMAIL",
                &format!("Email '{}' from {}: {}", mail.subject, mail.from_address, codes.join(", ")),
            );
        }

        self.repo.record_processed_mail(&ProcessedMail {
            internet_message_id: key,
            source_id: Some(mail.id.clone()),
            processed_at: None,
            outcome: serde_json::to_value(parsed.outcome)?.as_str().unwrap_or("JOBS").to_string(),
            from_address: Some(mail.from_address.clone()),
            subject: Some(mail.subject.clone()),
            jobs_json: serde_json::to_string(&records)?,
        })?;
        Ok(Handled::Done)
    }

    fn enqueue_parsed(&self, mail: &InboundMail, parsed: &ParsedEmail, default_priority: i32) -> Result<Vec<QueuedFromMail>> {
        let journalist = &parsed.journalist.surname;
        let mut notes = format!("Email: {}", mail.subject);
        if !parsed.warnings.is_empty() {
            let codes: Vec<&str> = parsed.warnings.iter().map(|w| w.code.as_str()).collect();
            notes.push_str(&format!(" [{}]", codes.join(", ")));
        }
        // Urgent mail goes ahead of the journalist's usual place in the queue.
        let priority = default_priority + if parsed.urgent { URGENT_PRIORITY_BOOST } else { 0 };

        let mut out = Vec::new();
        for job in parsed.jobs() {
            let slug = format!("{}_{}_{}", job.index_str, journalist, job.keyword);
            // Attachments are queued for a human until the pipeline can take
            // a file from the mailbox itself (plan P4.6).
            let (status, method) = match job.tier {
                Tier::Attachment => (JobStatus::ManualDownload, Some("attachment")),
                Tier::Locker => (job.status, Some("locker")),
                _ => (job.status, None),
            };
            let new = NewJob {
                url: job.url.clone(),
                slug: slug.clone(),
                journalist: journalist.clone(),
                keyword: job.keyword.clone(),
                index_str: job.index_str.clone(),
                priority,
                status,
                submitted_by_user_id: None,
                notes: Some(notes.clone()),
                email_source: Some(mail.from_address.clone()),
                email_message_id: Some(mail_key(mail)),
                extraction_method: method.map(String::from),
            };
            let result = self.repo.enqueue(&new, DEFAULT_DEDUP_WINDOW_HOURS)?;
            match &result {
                Enqueued::Created { id } => info!("EmailWatcher: queued job #{id} {slug} ({})", status.as_str()),
                Enqueued::DuplicateActive { existing_id } => {
                    info!("EmailWatcher: {} already queued as job #{existing_id}", job.url)
                }
                Enqueued::DuplicateRecent { existing_id } => {
                    info!("EmailWatcher: {} delivered recently as job #{existing_id}", job.url)
                }
            }
            out.push(QueuedFromMail {
                index_str: job.index_str.clone(),
                slug,
                url: job.url.clone(),
                status: status.as_str().to_string(),
                result,
            });
        }
        Ok(out)
    }
}

/// Priority added to jobs from a mail whose subject is urgent (plan P4.5).
pub const URGENT_PRIORITY_BOOST: i32 = 10;

enum Handled {
    Done,
    GaveUpEarlier,
}

/// One entry of `processed_mail.jobs_json`: what the summary reply (plan
/// P5.2) tells the journalist about each link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedFromMail {
    pub index_str: String,
    pub slug: String,
    pub url: String,
    pub status: String,
    pub result: Enqueued,
}

/// Idempotency key: the Message-ID, or the provider id for the rare message
/// that has none (a hand-crafted or broken sender).
pub fn mail_key(mail: &InboundMail) -> String {
    let mid = mail.internet_message_id.trim();
    if mid.is_empty() {
        format!("source:{}", mail.id)
    } else {
        mid.to_string()
    }
}