use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use omni_core::config::AppConfig;
use omni_core::health::{Check, HealthState};
use omni_core::models::{Enqueued, JobStatus, NewJob, ProcessedMail};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

use crate::assist::{Assist, LiveAssist};
use crate::graph::{GraphMailSource, RetryAfter};
use crate::mail::InboundMail;
use crate::parser::{ParsedEmail, Tier};
use crate::source::{MailGone, MailHeader, MailOutcome, MailSource};

/// New messages processed per poll. The rest wait for the next poll, oldest
/// change first; the checkpoint does not pass them.
const FETCH_LIMIT: usize = 20;

/// Headers listed per poll at most. Only reached when a person bulk-edits
/// thousands of mails at once (select all, mark read); they are all already
/// known or too old, so they cost a listing, not a fetch.
pub const LIST_LIMIT: usize = 5000;

/// Each poll lists from this far before the checkpoint, so a message whose
/// change time the server stamps slightly late is not stepped over. Mail seen
/// twice is harmless: `processed_mail` recognises it.
pub const CHECKPOINT_OVERLAP: chrono::Duration = chrono::Duration::minutes(15);

/// First poll of a mailbox (no checkpoint yet): look back this far. Mail the
/// old IMAP build already handled is in `processed_mail` and is not queued
/// again.
pub const FIRST_RUN_LOOKBACK: chrono::Duration = chrono::Duration::hours(24);

/// A mail received this long before the checkpoint is not ingested even when
/// it shows up as changed (someone flagged or moved a month-old mail). Long
/// enough for a Monday rescue of a Friday mail from Junk.
pub const MAX_MAIL_AGE: chrono::Duration = chrono::Duration::hours(72);

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
    /// Second opinion on journalist and keywords (plan P4.4).
    /// Shared with the admin panel, which may swap it (plan P4.22).
    assist: LiveAssist,
    /// Where video attachments are saved: `{dir}/{job_id}/source.{ext}`.
    /// Not under `temp/jobs`, which start-up sweeps for every job that is
    /// not running — a queued attachment job would lose its only source.
    attachments_dir: PathBuf,
}

/// What one poll did, for health and tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PollReport {
    /// Headers the listing returned (changed since the checkpoint).
    pub listed: usize,
    /// Already in `processed_mail`: not fetched again.
    pub already_seen: usize,
    /// Received before the age floor: not ingested.
    pub too_old: usize,
    /// Fetched in full, to be processed.
    pub fetched: usize,
    /// Read again on an operator's request (plan P4.25).
    pub reprocessed: usize,
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
    // and an operator should not have to read an AADSTS code to tell them apart.
    if lower.contains("graph login failed") || lower.contains("aadsts") {
        return "Microsoft sign-in rejected the app credentials".to_string();
    }
    if lower.contains("timed out") || lower.contains("connect") || lower.contains("dns") {
        return "cannot reach the mail server".to_string();
    }
    text.chars().take(120).collect()
}

impl EmailWatcher {
    /// The station mailbox, through Microsoft Graph (defect E-01, plan P4.7).
    pub fn new(config: AppConfig, repo: Repository) -> Self {
        let source: Arc<dyn MailSource> = Arc::new(GraphMailSource::new(config.graph.clone()));
        Self::with_source(config, repo, source)
    }

    /// A watcher over any mail source (Graph, or a test double).
    pub fn with_source(config: AppConfig, repo: Repository, source: Arc<dyn MailSource>) -> Self {
        let attachments_dir = config.resolve_path(&config.temp_path).join("attachments");
        let assist = LiveAssist::new(Assist::from_config(&config));
        Self {
            attachments_dir,
            assist,
            config,
            repo,
            source,
            health: None,
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Save attachments here instead of `{temp}/attachments`.
    pub fn with_attachments_dir(mut self, dir: PathBuf) -> Self {
        self.attachments_dir = dir;
        self
    }

    /// Use the daemon's live assist, which the admin panel can reconfigure.
    pub fn with_live_assist(mut self, assist: LiveAssist) -> Self {
        self.assist = assist;
        self
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

    pub async fn start_polling_loop(self: Arc<Self>, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
        info!("EmailWatcher: watching {}", self.source.describe());

        let poll_interval = Duration::from_secs(self.config.email_poll_interval_secs.max(10));
        let mut backoff = Backoff::default();

        loop {
            let wait = backoff.next_wait(poll_interval);
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("EmailWatcher: Received shutdown signal. Exiting watchdog loop.");
                    break;
                }
                _ = tokio::time::sleep(wait) => {
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
                            if let Some(check) = backoff.failed(&e, Instant::now()) {
                                self.report_health(check.with_error(format!("{e:#}")));
                            }
                        }
                        Ok(_) => {
                            backoff.succeeded();
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

    /// One poll (plan P4.8): list what changed since the checkpoint, fetch
    /// and process only what `processed_mail` has not seen, then move the
    /// checkpoint up to the last message that is settled.
    ///
    /// A message is recorded **only after** what it produced is persisted
    /// (defect E-02 marked everything `\Seen`, success or not, so a failed
    /// parse silently lost the links). The mailbox may be read-only: nothing
    /// here depends on a message being marked.
    pub async fn poll_once(&self) -> Result<PollReport> {
        let mut reprocessed = 0;
        self.reprocess_requested(&mut reprocessed).await?;
        let mut report = self.poll_changed().await?;
        report.reprocessed = reprocessed;
        Ok(report)
    }

    /// Mail an operator asked to have read again (plan P4.25): fetched by
    /// its mailbox id and parsed as new. Jobs it already has are not queued
    /// twice (URL dedup); only what is missing is added.
    async fn reprocess_requested(&self, done: &mut usize) -> Result<()> {
        for (key, source_id, attempts) in self.repo.pending_mail_reprocess()? {
            let give_up = |why: &str| {
                let _ = self.repo.finish_mail_reprocess(&key);
                let _ = self.repo.log_audit("ERROR", "EMAIL", &format!("Could not reprocess {key}: {why}"));
            };
            let mail = match self.source.fetch_mail(&source_id).await {
                Ok(m) => m,
                Err(e) if e.chain().any(|c| c.is::<MailGone>()) => {
                    give_up("it is no longer in the mailbox");
                    continue;
                }
                Err(e) => {
                    warn!("EmailWatcher: reprocess of {key} failed to fetch: {e:#}");
                    self.repo.note_mail_reprocess_failure(&key)?;
                    if attempts + 1 >= MAX_PROCESS_ATTEMPTS as i64 {
                        give_up(&short_reason(&e));
                    }
                    continue;
                }
            };
            let previous = self.repo.get_processed_mail(&key)?;
            self.repo.forget_processed_mail(&key)?;
            match self.process_mail(&mail).await {
                Ok(_) => {
                    self.repo.finish_mail_reprocess(&key)?;
                    let now = self.repo.get_processed_mail(&key)?;
                    let jobs = now
                        .as_ref()
                        .and_then(|p| serde_json::from_str::<Vec<serde_json::Value>>(&p.jobs_json).ok())
                        .map(|v| v.len())
                        .unwrap_or(0);
                    info!("EmailWatcher: reprocessed '{}': {jobs} job(s)", mail.subject);
                    let _ = self.repo.log_audit(
                        "INFO",
                        "EMAIL",
                        &format!(
                            "Reprocessed email '{}' from {}: {} ({jobs} job(s))",
                            mail.subject,
                            mail.from_address,
                            now.map(|p| p.outcome).unwrap_or_default()
                        ),
                    );
                    *done += 1;
                }
                Err(e) => {
                    // Put the old record back: the mail stays "handled"
                    // rather than being picked up half-done by a listing.
                    if let Some(p) = previous {
                        let _ = self.repo.record_processed_mail(&p);
                    }
                    error!("EmailWatcher: reprocessing '{}' failed: {e:#}", mail.subject);
                    self.repo.note_mail_reprocess_failure(&key)?;
                    if attempts + 1 >= MAX_PROCESS_ATTEMPTS as i64 {
                        give_up(&short_reason(&e));
                    }
                }
            }
        }
        Ok(())
    }

    /// List what changed since the checkpoint and process what is new.
    async fn poll_changed(&self) -> Result<PollReport> {
        let now = Utc::now();
        let key = self.source.checkpoint_key();
        let checkpoint = self.repo.get_mail_checkpoint(&key)?;
        let (since, received_floor) = match checkpoint {
            Some(c) => (c - CHECKPOINT_OVERLAP, c - MAX_MAIL_AGE),
            None => (now - FIRST_RUN_LOOKBACK, now - FIRST_RUN_LOOKBACK),
        };
        let headers = self.source.list_changed(since, LIST_LIMIT).await?;
        let mut report = PollReport {
            listed: headers.len(),
            ..Default::default()
        };
        if headers.len() >= LIST_LIMIT {
            warn!("EmailWatcher: listing hit {LIST_LIMIT} messages changed since {since}; the rest wait for later polls");
        }

        // The checkpoint may pass a message only once it is settled: done,
        // given up on, gone, or too old. One still being retried holds it,
        // though later messages are still processed (and recorded, so the
        // next poll skips them cheaply).
        let mut advance_to = checkpoint;
        let mut held = false;
        for h in &headers {
            let settled = self.handle(h, received_floor, &mut report).await?;
            if !settled {
                held = true;
            } else if !held && advance_to.map_or(true, |c| h.modified_at > c) {
                advance_to = Some(h.modified_at);
            }
        }
        if advance_to != checkpoint {
            if let Some(t) = advance_to {
                self.repo.set_mail_checkpoint(&key, t)?;
            }
        }
        if report.fetched > 0 {
            info!(
                "EmailWatcher: {} new message(s); {} processed, {} retrying, {} failed",
                report.fetched, report.processed, report.retrying, report.failed
            );
        }
        Ok(report)
    }

    /// Deal with one listed message. `Ok(true)` when it is settled and the
    /// checkpoint may pass it.
    async fn handle(&self, h: &MailHeader, received_floor: DateTime<Utc>, report: &mut PollReport) -> Result<bool> {
        let key = key_for(&h.internet_message_id, &h.id);
        // E-07, and the heart of read-only ingest: seen before, nothing to
        // do, whatever the mailbox says about read state.
        if self.repo.get_processed_mail(&key)?.is_some() {
            report.already_seen += 1;
            return Ok(true);
        }
        if h.received_at.is_some_and(|r| r < received_floor) {
            info!(
                "EmailWatcher: not ingesting '{}' from {}: received {}, before {received_floor}",
                h.subject,
                h.from_address,
                h.received_at.map(|r| r.to_rfc3339()).unwrap_or_default()
            );
            report.too_old += 1;
            return Ok(true);
        }
        if report.fetched >= FETCH_LIMIT {
            return Ok(false);
        }

        report.fetched += 1;
        let result = match self.source.fetch_mail(&h.id).await {
            Ok(mail) => self.process_mail(&mail).await.map(|handled| (mail, handled)),
            Err(e) if e.chain().any(|c| c.is::<MailGone>()) => {
                info!("EmailWatcher: '{}' left the inbox before it was read; skipped", h.subject);
                self.attempts.lock().unwrap().remove(&h.id);
                return Ok(true);
            }
            Err(e) => Err(e),
        };
        match result {
            Ok((_, Handled::GaveUpEarlier)) => Ok(true),
            Ok((mail, Handled::Done)) => {
                self.attempts.lock().unwrap().remove(&h.id);
                if let Err(e) = self.source.mark_processed(&mail.id, MailOutcome::Processed).await {
                    // The jobs and the processed_mail row exist; marking is
                    // only a courtesy to people reading the mailbox.
                    warn!("EmailWatcher: could not mark message {} processed: {e:#}", mail.id);
                }
                report.processed += 1;
                Ok(true)
            }
            Err(e) => Ok(self.note_failure(h, &key, &e, report).await),
        }
    }

    /// Count a failed attempt; after [`MAX_PROCESS_ATTEMPTS`] record the
    /// message as FAILED so it stops holding the checkpoint. Returns whether
    /// it is now settled.
    async fn note_failure(&self, h: &MailHeader, key: &str, e: &anyhow::Error, report: &mut PollReport) -> bool {
        let n = {
            let mut attempts = self.attempts.lock().unwrap();
            let n = attempts.entry(h.id.clone()).or_insert(0);
            *n += 1;
            *n
        };
        error!(
            "EmailWatcher: processing '{}' failed (attempt {n}/{MAX_PROCESS_ATTEMPTS}): {e:#}",
            h.subject
        );
        if n < MAX_PROCESS_ATTEMPTS {
            report.retrying += 1;
            return false;
        }
        let _ = self.repo.log_audit(
            "ERROR",
            "EMAIL",
            &format!("Gave up on email '{}' from {}: {}", h.subject, h.from_address, short_reason(e)),
        );
        if let Err(db) = self.repo.record_processed_mail(&ProcessedMail {
            internet_message_id: key.to_string(),
            source_id: Some(h.id.clone()),
            processed_at: None,
            outcome: MailOutcome::Failed.as_str().into(),
            from_address: Some(h.from_address.clone()),
            subject: Some(h.subject.clone()),
            jobs_json: "[]".into(),
        }) {
            // Not recorded: it must keep holding the checkpoint, or it is lost.
            warn!("EmailWatcher: could not record '{}' as failed: {db:#}", h.subject);
            report.retrying += 1;
            return false;
        }
        self.attempts.lock().unwrap().remove(&h.id);
        if let Err(e) = self.source.mark_processed(&h.id, MailOutcome::Failed).await {
            warn!("EmailWatcher: could not mark message {} failed: {e:#}", h.id);
        }
        report.failed += 1;
        true
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
        let groups = self.repo.list_groups()?;
        let parsed =
            crate::assist::interpret(mail, &roster, &groups, &self.config.parser, Some(self.assist.current().as_ref())).await;
        let default_priority = roster
            .iter()
            .find(|j| j.surname == parsed.journalist.surname)
            .map(|j| j.default_priority)
            .unwrap_or(0);
        let mut records = self.enqueue_parsed(mail, &parsed, default_priority)?;
        self.fetch_attachments(mail, &mut records).await;

        info!(
            "EmailWatcher: {} → {} ({:?}), {} job(s), {} new",
            key,
            parsed.journalist.surname,
            parsed.journalist.how,
            records.len(),
            records.iter().filter(|r| r.result.is_new()).count()
        );
        if !parsed.warnings.is_empty() {
            let codes: Vec<String> = parsed.warnings.iter().map(warning_label).collect();
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

    /// Download each newly queued attachment and release its job (plan P4.6).
    ///
    /// Never fails the message: the jobs already exist, and retrying the mail
    /// would only find them again. A failed download leaves its job as
    /// MANUAL_DOWNLOAD with an event saying why, which is where a human looks.
    async fn fetch_attachments(&self, mail: &InboundMail, records: &mut [QueuedFromMail]) {
        for rec in records.iter_mut() {
            let (Some(att_id), Enqueued::Created { id }) = (rec.attachment_id.clone(), rec.result.clone()) else {
                continue;
            };
            let name = mail
                .attachments
                .iter()
                .find(|a| a.id == att_id)
                .map(|a| a.name.clone())
                .unwrap_or_default();
            let dest = self
                .attachments_dir
                .join(id.to_string())
                .join(format!("source.{}", attachment_extension(&name)));
            let outcome = match self.source.download_attachment(&mail.id, &att_id, &dest).await {
                Ok(path) => self.repo.attach_source(id, &path.to_string_lossy()),
                Err(e) => Err(e),
            };
            match outcome {
                Ok(true) => {
                    info!("EmailWatcher: attachment '{name}' saved for job #{id}");
                    rec.status = JobStatus::Pending.as_str().into();
                }
                Ok(false) => info!("EmailWatcher: job #{id} was changed by MCR before its attachment arrived"),
                Err(e) => {
                    warn!("EmailWatcher: attachment '{name}' for job #{id} not saved: {e:#}");
                    let _ = self.repo.record_event(
                        id,
                        "WARN",
                        None,
                        &format!("Could not save the attachment '{name}' from the email ({}); download it from the mail by hand", short_reason(&e)),
                    );
                }
            }
        }
    }

    fn enqueue_parsed(&self, mail: &InboundMail, parsed: &ParsedEmail, default_priority: i32) -> Result<Vec<QueuedFromMail>> {
        let journalist = &parsed.journalist.surname;
        let mut notes = format!("Email: {}", mail.subject);
        if !parsed.warnings.is_empty() {
            let codes: Vec<String> = parsed.warnings.iter().map(warning_label).collect();
            notes.push_str(&format!(" [{}]", codes.join(", ")));
        }
        // Urgent mail goes ahead of the journalist's usual place in the queue.
        let priority = default_priority + if parsed.urgent { URGENT_PRIORITY_BOOST } else { 0 };

        let mut out = Vec::new();
        for job in parsed.jobs() {
            let slug = format!("{}_{}_{}", job.index_str, journalist, job.keyword);
            // Attachment jobs are parked as MANUAL_DOWNLOAD until their file
            // is on disk (`fetch_attachments`), so no worker can lease one
            // first. If the download fails they stay there, for MCR.
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
                group_code: parsed.group.as_ref().map(|g| g.code.clone()),
                max_videos: job.max_videos.map(i64::from),
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
                attachment_id: job.attachment_id.clone(),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
}

/// File extension for a saved attachment: the original one when it is a
/// plain short extension, else `bin` (ffprobe identifies content, not names).
fn attachment_extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".into())
}

/// A warning for people: the code, and for a suggested recipient the name
/// too ("JOURNALIST_SUGGESTED: Εύη"), which is what MCR acts on.
fn warning_label(w: &crate::parser::Warning) -> String {
    match (&w.detail, w.code.as_str()) {
        (Some(d), crate::parser::warnings::JOURNALIST_SUGGESTED) => format!("{}: {d}", w.code),
        _ => w.code.clone(),
    }
}

/// Idempotency key: the Message-ID, or the provider id for the rare message
/// that has none (a hand-crafted or broken sender).
pub fn mail_key(mail: &InboundMail) -> String {
    key_for(&mail.internet_message_id, &mail.id)
}

fn key_for(internet_message_id: &str, provider_id: &str) -> String {
    let mid = internet_message_id.trim();
    if mid.is_empty() {
        format!("source:{provider_id}")
    } else {
        mid.to_string()
    }
}
/// Consecutive failures before the mail check turns Degraded (plan P4.2).
/// One failed poll is a blip — a Graph 503, a DNS hiccup — and a panel that
/// flaps on every blip teaches operators to ignore it.
pub const DEGRADED_AFTER_FAILURES: u32 = 3;
/// An outage this long is Down, not Degraded.
pub const DOWN_AFTER: Duration = Duration::from_secs(600);
/// Ceiling for the exponential backoff between failing polls.
pub const MAX_BACKOFF: Duration = Duration::from_secs(600);

/// Poll pacing and health escalation across consecutive failures.
#[derive(Debug, Default)]
pub struct Backoff {
    failures: u32,
    since: Option<Instant>,
    retry_after: Option<Duration>,
}

impl Backoff {
    /// How long to wait before the next poll.
    pub fn next_wait(&self, poll: Duration) -> Duration {
        if self.failures == 0 {
            return poll;
        }
        let exp = poll.saturating_mul(1u32 << self.failures.min(10)).min(MAX_BACKOFF);
        exp.max(self.retry_after.unwrap_or_default())
    }

    /// Record a failure; returns the health to report, if it should change.
    pub fn failed(&mut self, e: &anyhow::Error, now: Instant) -> Option<Check> {
        self.failures += 1;
        let since = *self.since.get_or_insert(now);
        self.retry_after = e.chain().find_map(|c| c.downcast_ref::<RetryAfter>()).map(|r| r.0);
        if now.duration_since(since) >= DOWN_AFTER {
            Some(Check::down(short_reason(e)))
        } else if self.failures >= DEGRADED_AFTER_FAILURES {
            Some(Check::degraded(short_reason(e)))
        } else {
            None
        }
    }

    pub fn succeeded(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::health::Health;

    #[test]
    fn backoff_escalates_and_honours_retry_after() {
        let poll = Duration::from_secs(30);
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let err = anyhow::anyhow!("cannot reach Graph");

        assert_eq!(b.next_wait(poll), poll);
        assert!(b.failed(&err, t0).is_none(), "one blip must not flip the panel");
        assert_eq!(b.next_wait(poll), Duration::from_secs(60));
        assert!(b.failed(&err, t0).is_none());
        assert_eq!(b.failed(&err, t0).unwrap().state, Health::Degraded);
        assert!(b.next_wait(poll) <= MAX_BACKOFF);

        assert_eq!(b.failed(&err, t0 + DOWN_AFTER).unwrap().state, Health::Down);

        // A throttle longer than the backoff wins.
        let throttled = anyhow::Error::new(RetryAfter(Duration::from_secs(900))).context("fetch");
        b.failed(&throttled, t0 + DOWN_AFTER);
        assert_eq!(b.next_wait(poll), Duration::from_secs(900));

        b.succeeded();
        assert_eq!(b.next_wait(poll), poll);
    }
}