use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};

use omni_core::config::AppConfig;
use omni_core::health::{Check, HealthState};
use omni_core::models::JobStatus;
use omni_core::repository::Repository;

use crate::decontaminate::decontaminate_email_body;
use crate::imap_source::ImapMailSource;
use crate::interceptor::{intercept_volatile_urls, make_manual_slug};
use crate::llm::LlmClient;
use crate::mail::InboundMail;
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
    llm: Arc<LlmClient>,
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
        let llm = Arc::new(LlmClient::new(&config.ollama_endpoint, &config.ollama_model));
        Self {
            config,
            repo,
            llm,
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
                Ok(()) => {
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

    async fn process_mail(&self, mail: &InboundMail) -> Result<()> {
        let subject = &mail.subject;
        let from = &mail.from_address;

        info!("EmailWatcher: Processing email From: '{}', Subject: '{}'", from, subject);

        let body_text = mail.readable_body();
        let decontaminated = decontaminate_email_body(&body_text);

        // 1. Intercept volatile file-locker URLs (WeTransfer, TransferNow, AMNA)
        let volatile_urls = intercept_volatile_urls(&decontaminated);
        if !volatile_urls.is_empty() {
            info!("EmailWatcher: Intercepted {} volatile file-locker URL(s).", volatile_urls.len());
            for v_url in volatile_urls {
                let slug = make_manual_slug(&v_url);
                let _ = self.repo.add_job(
                    &v_url,
                    &slug,
                    "MCR",
                    "MANUAL_DOWNLOAD",
                    "1",
                    1, // High priority
                    JobStatus::ManualDownload,
                    None,
                    Some("Volatile file-locker intercepted from email"),
                    Some(from),
                );
            }
        }

        // 2. Format prompt for LLM
        let user_prompt = format!("Sender: {}\nSubject: {}\n\nBody:\n{}", from, subject, decontaminated);

        let llm_res = match self.llm.parse_email(&self.config.system_prompt, &user_prompt).await {
            Ok(res) => res,
            Err(e) => {
                warn!("LLM extraction failed: {}", e);
                let _ = self.repo.log_audit("WARN", "LLM", &format!("LLM parsing error for email '{}': {}", subject, e));
                return Err(e);
            }
        };

        // 3. Resolve journalist surname against database mappings
        let mut journalist = llm_res.journalist_surname.trim().to_uppercase();
        if let Ok(Some(mapped_surname)) = self.repo.find_journalist_by_email(from) {
            info!("Authoritative journalist mapping found: '{}' -> {}", from, mapped_surname);
            journalist = mapped_surname;
        }

        info!("LLM extracted {} jobs for journalist '{}'", llm_res.jobs.len(), journalist);

        // 4. Ingest parsed jobs into SQLite queue
        for job in llm_res.jobs {
            let index_str = job.index_str.trim().to_uppercase();
            let keyword = job.keyword.trim().to_uppercase();
            let confidence = job.confidence;

            // Formulate broadcast standardized slug: {index_str}_{journalist}_{keyword}
            let slug = format!("{}_{}_{}", index_str, journalist, keyword);

            let status = if keyword == "MANUAL_DOWNLOAD" {
                JobStatus::ManualDownload
            } else if confidence >= 0.7 {
                JobStatus::Pending
            } else {
                JobStatus::RequiresReview
            };

            let priority = if status == JobStatus::ManualDownload { 1 } else { 0 };

            match self.repo.add_job(
                &job.url,
                &slug,
                &journalist,
                &keyword,
                &index_str,
                priority,
                status,
                None,
                Some(&format!("Email subject: {}", subject)),
                Some(from),
            ) {
                Ok(id) => {
                    info!("Successfully enqueued Job #{} ({}) Status: {:?}", id, slug, status);
                }
                Err(e) => {
                    info!("Notice: Job for URL '{}' skipped (already queued): {}", job.url, e);
                }
            }
        }

        Ok(())
    }
}
