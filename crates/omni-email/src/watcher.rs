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
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS, MAIL_KEEP_DAYS};

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

/// Days short of `MAIL_KEEP_DAYS` that a mail must be received within to be ingested:
/// the handled-mail record that stops a re-ingest is purged after `MAIL_KEEP_DAYS`.
pub const MAIL_RECORD_MARGIN_DAYS: i64 = 7;

/// Consecutive processing failures before a message is given up on and left
/// unread for a human (plan P4.2: "Omni/Failed"). A transient failure (the
/// database briefly locked, the disk full) gets this many polls to clear.
pub const MAX_PROCESS_ATTEMPTS: u32 = 3;

/// What the watcher reads the mailbox with: the source and the day its
/// secret expires. Swapped as one when the administrator saves the mail
/// settings (P11.11c).
#[derive(Clone)]
pub struct MailSettings {
    pub source: Arc<dyn MailSource>,
    pub secret_expires: Option<chrono::NaiveDate>,
}

impl MailSettings {
    /// The Graph mailbox `graph` names.
    pub fn graph(graph: &omni_core::config::GraphConfig) -> Self {
        Self {
            source: Arc::new(GraphMailSource::new(graph.clone())),
            secret_expires: graph.client_secret_expires,
        }
    }
}

/// The mail settings in use, shared by the watcher and the admin panel
/// (P11.11c), and the pending automatic re-read of the mail given up on
/// (P11.11b). Cheap to clone.
///
/// A poll reads the settings once, at its start, and uses them to its end:
/// a header listed in one mailbox is never fetched from another.
#[derive(Clone)]
pub struct LiveMail {
    settings: Arc<tokio::sync::watch::Sender<MailSettings>>,
    /// Why the mail given up on should be read again after the next good
    /// poll: a restart, a tool update. `None`: nothing pending.
    failed_retry: Arc<Mutex<Option<String>>>,
}

impl LiveMail {
    pub fn new(settings: MailSettings) -> Self {
        Self {
            settings: Arc::new(tokio::sync::watch::Sender::new(settings)),
            failed_retry: Arc::new(Mutex::new(None)),
        }
    }

    pub fn current(&self) -> MailSettings {
        self.settings.borrow().clone()
    }

    /// Use `settings` from the next poll on; a waiting watcher polls at once.
    pub fn replace(&self, settings: MailSettings) {
        self.settings.send_replace(settings);
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<MailSettings> {
        self.settings.subscribe()
    }

    /// Read the mail given up on again after the next good poll (P11.11b).
    /// `cause` is what made it worth another try, for the audit log.
    pub fn arm_failed_retry(&self, cause: &str) {
        *self.failed_retry.lock().unwrap_or_else(|e| e.into_inner()) = Some(cause.to_string());
    }

    fn take_failed_retry(&self) -> Option<String> {
        self.failed_retry.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The cause recorded when the daemon has just started.
pub const RETRY_AFTER_RESTART: &str = "restart";
/// The cause recorded after a tool was updated or installed.
pub const RETRY_AFTER_TOOL_UPDATE: &str = "tool update";

/// The `mail` health line for a client secret that expires on `expires`, as of
/// `today` (P11.11d): amber from `config::SECRET_WARN_DAYS` days before, red from
/// `config::SECRET_RED_DAYS` days before and after. `None`: nothing to say.
pub fn secret_expiry_check(expires: Option<chrono::NaiveDate>, today: chrono::NaiveDate) -> Option<Check> {
    use omni_core::config::{SECRET_RED_DAYS, SECRET_WARN_DAYS};
    let expires = expires?;
    let left = (expires - today).num_days();
    let fix = "create a new client secret in Microsoft Entra and enter it, with its expiry date, under Administration → Mail";
    if left < 0 {
        Some(Check::down(format!("the mailbox client secret expired on {expires}: {fix}")))
    } else if left <= SECRET_RED_DAYS {
        Some(Check::down(format!("the mailbox client secret expires on {expires} (in {left} day(s)): {fix}")))
    } else if left <= SECRET_WARN_DAYS {
        Some(Check::degraded(format!("the mailbox client secret expires on {expires} (in {left} days): {fix}")))
    } else {
        None
    }
}

/// The detail of a failed poll when the client secret's date has passed: "expired
/// on <date>" and the fix, instead of only the sign-in error. `None` otherwise.
fn expired_secret_detail(expires: Option<chrono::NaiveDate>, today: chrono::NaiveDate) -> Option<String> {
    expires
        .filter(|e| *e < today)
        .and_then(|e| secret_expiry_check(Some(e), today))
        .and_then(|c| c.detail)
}

#[derive(Clone)]
pub struct EmailWatcher {
    config: AppConfig,
    repo: Repository,
    /// The source of this poll; [`Self::live`] holds the one in use.
    source: Arc<dyn MailSource>,
    secret_expires: Option<chrono::NaiveDate>,
    live: LiveMail,
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
        let secret_expires = config.graph.client_secret_expires;
        let live = LiveMail::new(MailSettings { source: source.clone(), secret_expires });
        Self {
            attachments_dir,
            assist,
            config,
            repo,
            source,
            secret_expires,
            live,
            health: None,
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Read the mailbox through `live`, which the admin panel can swap (P11.11c).
    pub fn with_live_mail(mut self, live: LiveMail) -> Self {
        let now = live.current();
        self.source = now.source;
        self.secret_expires = now.secret_expires;
        self.live = live;
        self
    }

    /// This watcher with the settings in use now, for one poll.
    fn snapshot(&self) -> Self {
        let now = self.live.current();
        let mut w = self.clone();
        w.source = now.source;
        w.secret_expires = now.secret_expires;
        w
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
        let mut settings_rx = self.live.subscribe();
        // The mail given up on before this start gets its one more try (P11.11b).
        self.live.arm_failed_retry(RETRY_AFTER_RESTART);
        let mut idle = false;

        loop {
            let wait = backoff.next_wait(poll_interval);
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("EmailWatcher: Received shutdown signal. Exiting watchdog loop.");
                    break;
                }
                // New settings from the admin panel (P11.11c): poll with them now, and a
                // backoff earned by the old ones no longer applies.
                changed = settings_rx.changed() => {
                    if changed.is_err() {
                        // The sender lives as long as `self.live`; this cannot happen.
                        break;
                    }
                    info!("EmailWatcher: mail settings changed; now watching {}", self.live.current().source.describe());
                    backoff.succeeded();
                }
                _ = tokio::time::sleep(wait) => {}
            }
            let poll = self.snapshot();
            if !poll.source.is_configured() {
                // Once, not every poll: a station may run without mail for good.
                if !idle {
                    warn!("EmailWatcher: the mailbox is not configured; waiting for its settings");
                    idle = true;
                }
                poll.report_health(Check::disabled("Mailbox"));
                continue;
            }
            idle = false;
            poll.poll_and_report(&mut backoff, poll_interval).await;
        }
    }

    /// One poll of the loop and what it tells the health state.
    async fn poll_and_report(&self, backoff: &mut Backoff, poll_interval: Duration) {
        match self.poll_once().await {
            Err(e) => {
                error!("EmailWatcher error: {:#}", e);
                let _ = self.repo.log_audit("ERROR", "EMAIL", &format!("Mail poll error: {}", short_reason(&e)));
                // The panel used to say "Mail: Active" through
                // exactly this (defect W-09). A short reason, not
                // the whole chain: this string is shown to an
                // operator, and it must not carry a credential.
                if let Some(mut check) = backoff.failed(&e, Instant::now()) {
                    // A secret past its date is the likeliest cause, and the one with a known fix.
                    if let Some(expired) = expired_secret_detail(self.secret_expires, chrono::Local::now().date_naive()) {
                        check.detail = Some(expired);
                    }
                    self.report_health(check.with_error(format!("{e:#}")));
                }
            }
            Ok(_) => {
                backoff.succeeded();
                // Every successful poll, for the metrics' "last poll age" (P6.5).
                if let Some(h) = &self.health {
                    h.note_mail_poll();
                }
                // A secret about to expire outranks "working" (P11.11d): the day it
                // expires, mail stops with no other warning.
                let today = chrono::Local::now().date_naive();
                self.report_health(secret_expiry_check(self.secret_expires, today).unwrap_or_else(|| {
                    Check::ok(format!("polling {} every {}s", self.source.describe(), poll_interval.as_secs()))
                }));
                if let Some(cause) = self.live.take_failed_retry() {
                    if let Err(e) = self.request_failed_mail_retry(&cause) {
                        // Kept for the next good poll: the database was busy, not the mail.
                        warn!("EmailWatcher: could not ask for the failed mail to be read again: {e:#}");
                        self.live.arm_failed_retry(&cause);
                    }
                }
            }
        }
    }

    /// Ask for one more reading of every mail given up on (FAILED) that is young
    /// enough (P11.11b); the next poll reads them. Young enough is the window of
    /// the jobs' automatic retries, `retry.max_age_mins`: nothing older is retried
    /// without a person. Returns how many were asked for.
    fn request_failed_mail_retry(&self, cause: &str) -> Result<usize> {
        let window = chrono::Duration::minutes(self.config.retry.max_age_mins.clamp(30, 360));
        let asked = self.repo.request_failed_mail_auto_retry(Utc::now() - window, cause)?;
        for (key, subject) in &asked {
            info!("EmailWatcher: {key} will be read again once ({cause})");
            let _ = self.repo.log_audit(
                "WARN",
                "EMAIL",
                &format!(
                    "Email '{}' that could not be read is read again automatically once (after a {cause})",
                    subject.as_deref().unwrap_or("")
                ),
            );
        }
        Ok(asked.len())
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
            // The system's own re-read (P11.11b) never delivers a link delivered under any name.
            let automatic = self.repo.mail_reprocess_is_automatic(&key)?;
            let previous = self.repo.get_processed_mail(&key)?;
            self.repo.forget_processed_mail(&key)?;
            match self.process_mail_as(&mail, automatic).await {
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
        // Handled-mail records are purged after MAIL_KEEP_DAYS; a mail older than
        // the safe part of that window could be re-ingested after its record is gone,
        // however old the checkpoint is (a poll that failed for months).
        let record_floor = now - chrono::Duration::days(MAIL_KEEP_DAYS - MAIL_RECORD_MARGIN_DAYS);
        let received_floor = received_floor.max(record_floor);
        // With no received time to judge by, a listing reaching back past that
        // window cannot be trusted either.
        let undated_too_old = since < record_floor;
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
            let settled = self.handle(h, received_floor, undated_too_old, &mut report).await?;
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
    async fn handle(
        &self,
        h: &MailHeader,
        received_floor: DateTime<Utc>,
        undated_too_old: bool,
        report: &mut PollReport,
    ) -> Result<bool> {
        let key = key_for(&h.internet_message_id, &h.id);
        // E-07, and the heart of read-only ingest: seen before, nothing to
        // do, whatever the mailbox says about read state.
        if self.repo.get_processed_mail(&key)?.is_some() {
            report.already_seen += 1;
            return Ok(true);
        }
        if h.received_at.map_or(undated_too_old, |r| r < received_floor) {
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
            received_at: h.received_at,
            ..Default::default()
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

    /// Handle one mail that did not come from a listing, such as a saved
    /// `.eml` (plan P7.9): the same steps as a poll (parse, queue, save the
    /// attachments, record the mail and its text), without touching a
    /// mailbox. A mail handled before is left alone.
    pub async fn ingest(&self, mail: &InboundMail) -> Result<Ingested> {
        if self.repo.get_processed_mail(&mail_key(mail))?.is_some() {
            return Ok(Ingested::AlreadyHandled);
        }
        self.process_mail(mail).await?;
        Ok(Ingested::Processed)
    }

    /// Parse one message and queue its jobs (plan P4.5).
    ///
    /// Returns only after every job is persisted; any error leaves the
    /// message for the next poll. A retry after a partial failure is safe: the
    /// jobs already created come back from `enqueue` as `DuplicateActive`.
    async fn process_mail(&self, mail: &InboundMail) -> Result<Handled> {
        self.process_mail_as(mail, false).await
    }

    /// [`Self::process_mail`]; `automatic`: the system's own re-read of a mail it
    /// gave up on, whose jobs are queued with [`Repository::enqueue_automatic`].
    async fn process_mail_as(&self, mail: &InboundMail, automatic: bool) -> Result<Handled> {
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
        let mut records = self.enqueue_parsed(mail, &parsed, default_priority, automatic)?;
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
            // What the MCR mail view shows (plan P7.6).
            received_at: mail.received_at,
            from_name: Some(mail.from_name.clone()),
            to: mail.to.clone(),
            cc: mail.cc.clone(),
            body_text: Some(crate::mail_view::stored_text(&mail.readable_body())),
            attachments_json: (!mail.attachments.is_empty())
                .then(|| serde_json::to_string(&mail.attachments))
                .transpose()?,
            parse_json: Some(serde_json::to_string(&crate::mail_view::MailParseSummary::of(&parsed))?),
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

    fn enqueue_parsed(
        &self,
        mail: &InboundMail,
        parsed: &ParsedEmail,
        default_priority: i32,
        automatic: bool,
    ) -> Result<Vec<QueuedFromMail>> {
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
                parent_job_id: None,
                queued_by_operator: false,
            };
            let result = if automatic {
                self.repo.enqueue_automatic(&new, DEFAULT_DEDUP_WINDOW_HOURS)?
            } else {
                self.repo.enqueue(&new, DEFAULT_DEDUP_WINDOW_HOURS)?
            };
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
/// Defined in omni-core, where the desk's "late" rule needs it too (P11.3b).
pub use omni_core::models::URGENT_PRIORITY_BOOST;

enum Handled {
    Done,
    GaveUpEarlier,
}

/// What [`EmailWatcher::ingest`] did with a mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingested {
    /// Parsed; its jobs (if any) are queued and the mail is recorded.
    Processed,
    /// Recorded before: nothing was done.
    AlreadyHandled,
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

    /// A mailbox that lists nothing, or fails to list.
    struct ScriptedSource {
        fail: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl MailSource for ScriptedSource {
        fn describe(&self) -> String {
            "scripted".into()
        }
        fn checkpoint_key(&self) -> String {
            "scripted:test".into()
        }
        fn is_configured(&self) -> bool {
            true
        }
        async fn list_changed(&self, _: DateTime<Utc>, _: usize) -> Result<Vec<MailHeader>> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("cannot reach the mailbox");
            }
            Ok(Vec::new())
        }
        async fn fetch_mail(&self, id: &str) -> Result<InboundMail> {
            anyhow::bail!("no mail {id}")
        }
        async fn mark_processed(&self, _: &str, _: MailOutcome) -> Result<()> {
            Ok(())
        }
        async fn download_attachment(&self, _: &str, _: &str, _: &std::path::Path) -> Result<PathBuf> {
            anyhow::bail!("none")
        }
        async fn reply(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn health(&self) -> crate::source::MailHealth {
            crate::source::MailHealth { ok: true, detail: "scripted".into() }
        }
    }

    #[tokio::test]
    async fn a_successful_poll_stamps_the_poll_time_and_a_failed_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let health = HealthState::new();
        let source = Arc::new(ScriptedSource { fail: std::sync::atomic::AtomicBool::new(true) });
        let watcher = EmailWatcher::with_source(AppConfig::default(), repo, source.clone()).with_health(health.clone());
        let mut backoff = Backoff::default();
        let every = Duration::from_secs(30);

        watcher.poll_and_report(&mut backoff, every).await;
        assert_eq!(health.last_mail_poll(), None, "a failed poll is not a poll");

        source.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        watcher.poll_and_report(&mut backoff, every).await;
        let first = health.last_mail_poll().expect("a successful poll stamps the time");

        source.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        watcher.poll_and_report(&mut backoff, every).await;
        assert_eq!(health.last_mail_poll(), Some(first), "a failed poll leaves it as it was");
    }

    /// Lists the given headers; every fetch says the mail is gone, so a mail that
    /// passes the age gate shows up as `fetched` and nothing else happens to it.
    struct ListSource {
        headers: Vec<MailHeader>,
    }

    #[async_trait::async_trait]
    impl MailSource for ListSource {
        fn describe(&self) -> String {
            "list".into()
        }
        fn checkpoint_key(&self) -> String {
            "list:test".into()
        }
        fn is_configured(&self) -> bool {
            true
        }
        async fn list_changed(&self, _: DateTime<Utc>, _: usize) -> Result<Vec<MailHeader>> {
            Ok(self.headers.clone())
        }
        async fn fetch_mail(&self, id: &str) -> Result<InboundMail> {
            Err(MailGone(id.to_string()).into())
        }
        async fn mark_processed(&self, _: &str, _: MailOutcome) -> Result<()> {
            Ok(())
        }
        async fn download_attachment(&self, _: &str, _: &str, _: &std::path::Path) -> Result<PathBuf> {
            anyhow::bail!("none")
        }
        async fn reply(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn health(&self) -> crate::source::MailHealth {
            crate::source::MailHealth { ok: true, detail: "list".into() }
        }
    }

    fn header(n: u32, received_ago: Option<chrono::Duration>) -> MailHeader {
        MailHeader {
            id: format!("id{n}"),
            internet_message_id: format!("<m{n}@news.example>"),
            subject: format!("Subject {n}"),
            from_address: "sender@news.example".into(),
            received_at: received_ago.map(|a| Utc::now() - a),
            modified_at: Utc::now(),
        }
    }

    /// One poll over `headers` with a checkpoint `checkpoint_age` old; the report.
    async fn poll_with(headers: Vec<MailHeader>, checkpoint_age: chrono::Duration) -> PollReport {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        repo.set_mail_checkpoint("list:test", Utc::now() - checkpoint_age).unwrap();
        let watcher = EmailWatcher::with_source(AppConfig::default(), repo, Arc::new(ListSource { headers }));
        watcher.poll_changed().await.unwrap()
    }

    #[tokio::test]
    async fn after_a_long_outage_a_mail_older_than_the_record_window_is_not_ingested() {
        // Checkpoint 100 days old: the old floor (checkpoint - 72 h) would accept a mail 95 days old.
        let r = poll_with(vec![header(1, Some(chrono::Duration::days(95)))], chrono::Duration::days(100)).await;
        assert_eq!((r.too_old, r.fetched), (1, 0));
    }

    #[tokio::test]
    async fn after_a_long_outage_an_undated_header_is_not_ingested() {
        let r = poll_with(vec![header(2, None)], chrono::Duration::days(100)).await;
        assert_eq!((r.too_old, r.fetched), (1, 0));
    }

    #[tokio::test]
    async fn after_a_long_outage_a_fresh_mail_is_still_ingested() {
        let r = poll_with(vec![header(3, Some(chrono::Duration::hours(2)))], chrono::Duration::days(100)).await;
        assert_eq!((r.too_old, r.fetched), (0, 1));
    }

    #[tokio::test]
    async fn with_a_recent_checkpoint_nothing_changes() {
        let hour = chrono::Duration::hours(1);
        let r = poll_with(
            vec![
                header(4, Some(chrono::Duration::hours(2))),
                header(5, Some(chrono::Duration::days(5))),
                header(6, None),
            ],
            hour,
        )
        .await;
        // 2 h old: in; 5 days old: past the 72 h age; undated: still let in, as before.
        assert_eq!((r.too_old, r.fetched), (1, 2));
    }

    #[test]
    fn the_secret_expiry_turns_the_mail_line_amber_at_thirty_days_and_red_at_seven() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        let state = |days: i64| secret_expiry_check(Some(today + chrono::Duration::days(days)), today).map(|c| c.state);
        assert_eq!(secret_expiry_check(None, today).map(|c| c.state), None, "no date, nothing to say");
        assert_eq!(state(31), None);
        assert_eq!(state(30), Some(Health::Degraded));
        assert_eq!(state(8), Some(Health::Degraded));
        assert_eq!(state(7), Some(Health::Down));
        assert_eq!(state(0), Some(Health::Down));
        let expired = secret_expiry_check(Some(today - chrono::Duration::days(1)), today).unwrap();
        assert_eq!(expired.state, Health::Down);
        assert!(expired.detail.unwrap().contains("expired on 2026-11-18"));
    }

    /// A mailbox that names itself and counts its listings.
    struct Named {
        name: &'static str,
        listed: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl MailSource for Named {
        fn describe(&self) -> String {
            self.name.into()
        }
        fn checkpoint_key(&self) -> String {
            format!("named:{}", self.name)
        }
        fn is_configured(&self) -> bool {
            true
        }
        async fn list_changed(&self, _: DateTime<Utc>, _: usize) -> Result<Vec<MailHeader>> {
            self.listed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Vec::new())
        }
        async fn fetch_mail(&self, id: &str) -> Result<InboundMail> {
            anyhow::bail!("no mail {id}")
        }
        async fn mark_processed(&self, _: &str, _: MailOutcome) -> Result<()> {
            Ok(())
        }
        async fn download_attachment(&self, _: &str, _: &str, _: &std::path::Path) -> Result<PathBuf> {
            anyhow::bail!("none")
        }
        async fn reply(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn health(&self) -> crate::source::MailHealth {
            crate::source::MailHealth { ok: true, detail: self.name.into() }
        }
    }

    fn named(name: &'static str) -> Arc<Named> {
        Arc::new(Named { name, listed: std::sync::atomic::AtomicUsize::new(0) })
    }

    #[tokio::test]
    async fn new_mail_settings_are_used_from_the_next_poll_and_a_poll_keeps_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let (old, new) = (named("old-box"), named("new-box"));
        let live = LiveMail::new(MailSettings { source: old.clone(), secret_expires: None });
        let watcher = EmailWatcher::with_source(AppConfig::default(), repo, named("unused")).with_live_mail(live.clone());

        let running = watcher.snapshot();
        live.replace(MailSettings { source: new.clone(), secret_expires: chrono::NaiveDate::from_ymd_opt(2030, 1, 1) });
        // The poll that started before the save ends with the mailbox it started with.
        running.poll_once().await.unwrap();
        assert_eq!(running.source.describe(), "old-box");
        assert_eq!(old.listed.load(std::sync::atomic::Ordering::SeqCst), 1);

        let next = watcher.snapshot();
        next.poll_once().await.unwrap();
        assert_eq!(next.source.describe(), "new-box");
        assert_eq!(next.secret_expires, chrono::NaiveDate::from_ymd_opt(2030, 1, 1));
        assert_eq!(new.listed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(old.listed.load(std::sync::atomic::Ordering::SeqCst), 1, "the old mailbox is not read again");
    }

    fn record_failed(repo: &Repository, key: &str, received_ago: chrono::Duration) {
        repo.record_processed_mail(&ProcessedMail {
            internet_message_id: key.into(),
            source_id: Some(format!("id-{key}")),
            outcome: MailOutcome::Failed.as_str().into(),
            subject: Some("Πλάνα".into()),
            received_at: Some(Utc::now() - received_ago),
            ..Default::default()
        })
        .unwrap();
    }

    #[tokio::test]
    async fn a_failed_mail_is_asked_for_again_after_the_first_good_poll_and_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        record_failed(&repo, "<young@news.example>", chrono::Duration::minutes(20));
        record_failed(&repo, "<old@news.example>", chrono::Duration::hours(3));
        let source = Arc::new(ScriptedSource { fail: std::sync::atomic::AtomicBool::new(true) });
        let watcher = EmailWatcher::with_source(AppConfig::default(), repo.clone(), source.clone());
        let pending = || repo.pending_mail_reprocess().unwrap().into_iter().map(|(k, _, _)| k).collect::<Vec<_>>();
        let (mut backoff, every) = (Backoff::default(), Duration::from_secs(30));

        watcher.live.arm_failed_retry(RETRY_AFTER_RESTART);
        watcher.poll_and_report(&mut backoff, every).await;
        assert!(pending().is_empty(), "a failed poll asks for nothing");

        source.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        watcher.poll_and_report(&mut backoff, every).await;
        // Older than retry.max_age_mins (2 h): left for a person.
        assert_eq!(pending(), vec!["<young@news.example>"]);

        // The reading fails again (this mailbox has no such mail) and gives up; a tool
        // update later does not give it a second chance.
        for _ in 0..MAX_PROCESS_ATTEMPTS {
            watcher.poll_and_report(&mut backoff, every).await;
        }
        assert!(pending().is_empty());
        assert_eq!(repo.get_processed_mail("<young@news.example>").unwrap().unwrap().outcome, "FAILED");
        watcher.live.arm_failed_retry(RETRY_AFTER_TOOL_UPDATE);
        watcher.poll_and_report(&mut backoff, every).await;
        assert!(pending().is_empty(), "once per mail");
    }

    /// Lists nothing; fetching any id returns `mail`.
    struct OneMail {
        mail: InboundMail,
    }

    #[async_trait::async_trait]
    impl MailSource for OneMail {
        fn describe(&self) -> String {
            "one".into()
        }
        fn checkpoint_key(&self) -> String {
            "one:test".into()
        }
        fn is_configured(&self) -> bool {
            true
        }
        async fn list_changed(&self, _: DateTime<Utc>, _: usize) -> Result<Vec<MailHeader>> {
            Ok(Vec::new())
        }
        async fn fetch_mail(&self, _: &str) -> Result<InboundMail> {
            Ok(self.mail.clone())
        }
        async fn mark_processed(&self, _: &str, _: MailOutcome) -> Result<()> {
            Ok(())
        }
        async fn download_attachment(&self, _: &str, _: &str, _: &std::path::Path) -> Result<PathBuf> {
            anyhow::bail!("none")
        }
        async fn reply(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn health(&self) -> crate::source::MailHealth {
            crate::source::MailHealth { ok: true, detail: "one".into() }
        }
    }

    /// Review round 2: MCR queued the failed mail's link by hand under the journalist and it
    /// was delivered; the system's re-read (which files the mail under MCR) makes no second job.
    #[tokio::test]
    async fn an_automatic_re_read_does_not_deliver_a_link_mcr_already_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let url = "https://www.youtube.com/watch?v=handqueued01";
        let key = "<retry@news.example>";
        record_failed(&repo, key, chrono::Duration::minutes(20));
        let id = repo.enqueue(&NewJob::new(url, "1_NIKOLAOU_LIMANI", "NIKOLAOU"), 24).unwrap().job_id();
        assert!(repo.lease_job_by_id(id, "host:1:0", 180).unwrap());
        assert!(repo.finish(id, "host:1:0", JobStatus::Completed, None, None, None).unwrap());

        let mail = InboundMail {
            id: format!("id-{key}"),
            internet_message_id: key.into(),
            from_address: "stranger@elsewhere.example".into(),
            subject: "Πλάνα".into(),
            received_at: Some(Utc::now()),
            body_text: format!("1. ΛΙΜΑΝΙ\n{url}\n"),
            ..Default::default()
        };
        let watcher = EmailWatcher::with_source(AppConfig::default(), repo.clone(), Arc::new(OneMail { mail }));
        let (mut backoff, every) = (Backoff::default(), Duration::from_secs(30));
        watcher.live.arm_failed_retry(RETRY_AFTER_RESTART);
        watcher.poll_and_report(&mut backoff, every).await; // asks for the re-read
        watcher.poll_and_report(&mut backoff, every).await; // re-reads it

        let read = repo.get_processed_mail(key).unwrap().unwrap();
        assert_ne!(read.outcome, "FAILED", "the mail was read again");
        assert!(read.jobs_json.contains("duplicate_recent"), "{}", read.jobs_json);
        assert_eq!(repo.get_all_jobs().unwrap().len(), 1, "no second job, so no second file");
    }

    #[tokio::test]
    async fn a_failed_poll_with_an_expired_secret_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let health = HealthState::new();
        let source = Arc::new(ScriptedSource { fail: std::sync::atomic::AtomicBool::new(true) });
        let past = chrono::Local::now().date_naive() - chrono::Duration::days(2);
        let live = LiveMail::new(MailSettings { source, secret_expires: Some(past) });
        let watcher = EmailWatcher::with_source(AppConfig::default(), repo, named("unused"))
            .with_live_mail(live)
            .with_health(health.clone());
        let mut backoff = Backoff::default();
        for _ in 0..DEGRADED_AFTER_FAILURES {
            watcher.poll_and_report(&mut backoff, Duration::from_secs(30)).await;
        }
        let check = health.all().into_iter().find(|(n, _)| n == omni_core::health::checks::MAIL).unwrap().1;
        assert!(check.detail.as_deref().unwrap_or("").contains(&format!("expired on {past}")), "{check:?}");
        assert!(check.last_error.is_some(), "the sign-in error is still kept");
    }

    #[tokio::test]
    async fn a_good_poll_with_a_secret_about_to_expire_is_reported_red() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let health = HealthState::new();
        let source = Arc::new(ScriptedSource { fail: std::sync::atomic::AtomicBool::new(false) });
        let soon = chrono::Local::now().date_naive() + chrono::Duration::days(3);
        let live = LiveMail::new(MailSettings { source, secret_expires: Some(soon) });
        let watcher = EmailWatcher::with_source(AppConfig::default(), repo, named("unused"))
            .with_live_mail(live)
            .with_health(health.clone());
        watcher.poll_and_report(&mut Backoff::default(), Duration::from_secs(30)).await;
        let check = health.all().into_iter().find(|(n, _)| n == omni_core::health::checks::MAIL).unwrap().1;
        assert_eq!(check.state, Health::Down, "{check:?}");
        assert!(health.last_mail_poll().is_some(), "the poll itself worked");
    }

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