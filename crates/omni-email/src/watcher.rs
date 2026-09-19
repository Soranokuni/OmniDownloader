use anyhow::{anyhow, Context, Result};
use mailparse::{parse_mail, ParsedMail};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

use omni_core::config::AppConfig;
use omni_core::models::JobStatus;
use omni_core::repository::Repository;

use crate::decontaminate::decontaminate_email_body;
use crate::interceptor::{intercept_volatile_urls, make_manual_slug};
use crate::llm::LlmClient;

#[derive(Clone)]
pub struct EmailWatcher {
    config: AppConfig,
    repo: Repository,
    llm: Arc<LlmClient>,
}

impl EmailWatcher {
    pub fn new(config: AppConfig, repo: Repository) -> Self {
        let llm = Arc::new(LlmClient::new(&config.ollama_endpoint, &config.ollama_model));
        Self { config, repo, llm }
    }

    pub fn test_connection(server: &str, port: u16, email: &str, pass: &str) -> Result<()> {
        let client = imap::ClientBuilder::new(server, port)
            .connect()
            .with_context(|| format!("Failed to connect to IMAP {}:{}", server, port))?;

        let mut session = client
            .login(email, pass)
            .map_err(|(e, _)| anyhow!("IMAP login failed: {}", e))?;

        let _ = session.logout();
        Ok(())
    }

    pub async fn start_polling_loop(self: Arc<Self>, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
        info!(
            "EmailWatcher: Starting IMAP watchdog on {} ({}:{})",
            self.config.email_provider, self.config.imap_server, self.config.imap_port
        );

        let poll_interval = Duration::from_secs(self.config.email_poll_interval_secs.max(10));

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("EmailWatcher: Received shutdown signal. Exiting watchdog loop.");
                    break;
                }
                _ = tokio::time::sleep(poll_interval) => {
                    if self.config.email_address.is_empty() || self.config.email_password.is_empty() {
                        warn!("EmailWatcher: Credentials not configured. Sleeping...");
                        continue;
                    }

                    let watcher = self.clone();
                    let res = tokio::task::spawn_blocking(move || watcher.poll_inbox_sync()).await;
                    match res {
                        Ok(Err(e)) => {
                            error!("EmailWatcher error: {:#}", e);
                            let _ = self.repo.log_audit("ERROR", "EMAIL", &format!("IMAP poll error: {}", e));
                        }
                        Err(join_err) => {
                            error!("EmailWatcher task error: {}", join_err);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn poll_inbox_sync(&self) -> Result<()> {
        let server = &self.config.imap_server;
        let port = self.config.imap_port;

        let client = imap::ClientBuilder::new(server.as_str(), port)
            .connect()
            .with_context(|| format!("Failed IMAP connect with {}:{}", server, port))?;

        let mut session = client
            .login(&self.config.email_address, &self.config.email_password)
            .map_err(|(e, _)| anyhow!("IMAP login failed for {}: {}", self.config.email_address, e))?;

        session
            .select("INBOX")
            .map_err(|e| anyhow!("Select INBOX failed: {}", e))?;

        let unseen_ids = session
            .search("UNSEEN")
            .map_err(|e| anyhow!("IMAP Search UNSEEN failed: {}", e))?;

        if !unseen_ids.is_empty() {
            info!("EmailWatcher: Found {} unread email(s). Processing...", unseen_ids.len());

            for id in unseen_ids {
                let id_str = id.to_string();
                let messages = match session.fetch(&id_str, "RFC822") {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("Fetch error for ID {}: {}", id, e);
                        continue;
                    }
                };

                for msg in messages.iter() {
                    if let Some(body_bytes) = msg.body() {
                        let rt = tokio::runtime::Handle::current();
                        if let Err(e) = rt.block_on(self.process_raw_email(body_bytes)) {
                            error!("Failed processing email #{}: {:#}", id, e);
                        }
                    }
                }

                // Mark as seen on mail server
                let _ = session.store(&id_str, "+FLAGS (\\Seen)");
            }
        }

        let _ = session.logout();
        Ok(())
    }

    async fn process_raw_email(&self, raw_bytes: &[u8]) -> Result<()> {
        let parsed = parse_mail(raw_bytes).context("Failed parsing RFC822 MIME message")?;

        let subject = parsed
            .headers
            .iter()
            .find(|h| h.get_key().eq_ignore_ascii_case("subject"))
            .map(|h| h.get_value())
            .unwrap_or_default();

        let from = parsed
            .headers
            .iter()
            .find(|h| h.get_key().eq_ignore_ascii_case("from"))
            .map(|h| h.get_value())
            .unwrap_or_default();

        info!("EmailWatcher: Processing email From: '{}', Subject: '{}'", from, subject);

        let body_text = extract_body_text(&parsed);
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
                    Some(&from),
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
        if let Ok(Some(mapped_surname)) = self.repo.find_journalist_by_email(&from) {
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
                Some(&from),
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

fn extract_body_text(mail: &ParsedMail) -> String {
    if mail.subparts.is_empty() {
        let content_type = mail.ctype.mimetype.to_lowercase();
        if content_type.contains("text/plain") || content_type.contains("text/html") {
            let body_bytes = mail.get_body_raw().unwrap_or_default();
            decode_text_with_charset(&body_bytes, &mail.ctype.charset)
        } else {
            String::new()
        }
    } else {
        // Multipart: look for text/plain first, then text/html
        for part in &mail.subparts {
            if part.ctype.mimetype.to_lowercase() == "text/plain" {
                let body_bytes = part.get_body_raw().unwrap_or_default();
                return decode_text_with_charset(&body_bytes, &part.ctype.charset);
            }
        }
        for part in &mail.subparts {
            if part.ctype.mimetype.to_lowercase() == "text/html" {
                let body_bytes = part.get_body_raw().unwrap_or_default();
                return decode_text_with_charset(&body_bytes, &part.ctype.charset);
            }
        }
        String::new()
    }
}

fn decode_text_with_charset(bytes: &[u8], charset: &str) -> String {
    let lower = charset.to_lowercase();
    if lower.contains("iso-8859-7") || lower.contains("greek") {
        let (cow, _, _) = encoding_rs::ISO_8859_7.decode(bytes);
        cow.to_string()
    } else if lower.contains("windows-1253") || lower.contains("cp1253") {
        let (cow, _, _) = encoding_rs::WINDOWS_1253.decode(bytes);
        cow.to_string()
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}
