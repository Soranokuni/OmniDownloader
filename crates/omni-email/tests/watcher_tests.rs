//! The watcher against a scripted mail source (plan P4.1, P4.8).
//!
//! Regression for E-02: the old IMAP loop marked every message `\Seen` after
//! the processing loop, whether or not processing worked, so a failure lost
//! the journalist's links without trace.
//!
//! The fake behaves like the station's `Mail.Read` mailbox: marking changes
//! nothing it lists. Whatever the watcher knows, it knows from the database.

use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, SubsecRound, Utc};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use omni_core::config::AppConfig;
use omni_core::models::JobStatus;
use omni_core::repository::Repository;
use omni_email::mail::{AttachmentMeta, InboundMail};
use omni_email::source::{MailGone, MailHeader, MailHealth, MailOutcome, MailSource};
use omni_email::watcher::{EmailWatcher, FIRST_RUN_LOOKBACK, MAX_PROCESS_ATTEMPTS, URGENT_PRIORITY_BOOST};

struct Entry {
    mail: InboundMail,
    modified_at: DateTime<Utc>,
}

#[derive(Default)]
struct FakeSource {
    inbox: Mutex<Vec<Entry>>,
    marks: Mutex<Vec<(String, MailOutcome)>>,
    fetches: Mutex<Vec<String>>,
    /// `since` of every listing, in order.
    listings: Mutex<Vec<DateTime<Utc>>>,
    fail_downloads: std::sync::atomic::AtomicBool,
    /// Fetching these ids fails (a Graph error for that one message).
    fail_fetch: Mutex<HashSet<String>>,
    /// These ids are listed but gone by the time they are fetched.
    vanished: Mutex<HashSet<String>>,
}

impl FakeSource {
    fn with(mails: Vec<InboundMail>) -> Arc<Self> {
        let src = Arc::new(Self::default());
        for m in mails {
            src.arrive(m);
        }
        src
    }
    /// A message changes now: it arrives, or is moved into the inbox. Each
    /// change is a second after the previous one, all within the last hour.
    fn arrive(&self, mail: InboundMail) {
        let mut inbox = self.inbox.lock().unwrap();
        let modified_at = inbox
            .iter()
            .map(|e| e.modified_at)
            .max()
            .map(|t| t + Duration::seconds(1))
            .unwrap_or_else(|| (Utc::now() - Duration::hours(1)).trunc_subsecs(0));
        inbox.push(Entry { mail, modified_at });
    }
    fn arrive_at(&self, mail: InboundMail, modified_at: DateTime<Utc>) {
        self.inbox.lock().unwrap().push(Entry { mail, modified_at });
    }
    fn marks(&self) -> Vec<(String, MailOutcome)> {
        self.marks.lock().unwrap().clone()
    }
    fn fetches(&self) -> Vec<String> {
        self.fetches.lock().unwrap().clone()
    }
    /// The server shows a message again under a new provider id (a mail
    /// dragged back to the inbox, a Graph move that failed after queueing).
    fn redeliver(&self, mut mail: InboundMail, new_id: &str) {
        mail.id = new_id.into();
        self.arrive(mail);
    }
}

#[async_trait]
impl MailSource for FakeSource {
    fn describe(&self) -> String {
        "fake".into()
    }
    fn checkpoint_key(&self) -> String {
        "fake:ingest@example.gr".into()
    }
    fn is_configured(&self) -> bool {
        true
    }
    async fn list_changed(&self, since: DateTime<Utc>, max: usize) -> Result<Vec<MailHeader>> {
        self.listings.lock().unwrap().push(since);
        let inbox = self.inbox.lock().unwrap();
        let mut out: Vec<MailHeader> = inbox
            .iter()
            .filter(|e| e.modified_at >= since)
            .map(|e| MailHeader {
                id: e.mail.id.clone(),
                internet_message_id: e.mail.internet_message_id.clone(),
                subject: e.mail.subject.clone(),
                from_address: e.mail.from_address.clone(),
                received_at: e.mail.received_at,
                modified_at: e.modified_at,
            })
            .collect();
        out.sort_by_key(|h| h.modified_at);
        out.truncate(max);
        Ok(out)
    }
    async fn fetch_mail(&self, id: &str) -> Result<InboundMail> {
        self.fetches.lock().unwrap().push(id.to_string());
        if self.vanished.lock().unwrap().contains(id) {
            return Err(anyhow::Error::new(MailGone(id.to_string())));
        }
        if self.fail_fetch.lock().unwrap().contains(id) {
            bail!("Graph returned 500 Internal Server Error: fetching {id}");
        }
        let inbox = self.inbox.lock().unwrap();
        match inbox.iter().rev().find(|e| e.mail.id == id) {
            Some(e) => Ok(e.mail.clone()),
            None => Err(anyhow::Error::new(MailGone(id.to_string()))),
        }
    }
    async fn mark_processed(&self, mail_id: &str, outcome: MailOutcome) -> Result<()> {
        // Recorded for the assertions; like Mail.Read, it changes nothing listed.
        self.marks.lock().unwrap().push((mail_id.to_string(), outcome));
        Ok(())
    }
    async fn download_attachment(&self, _: &str, att: &str, dest: &Path) -> Result<PathBuf> {
        if self.fail_downloads.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("connection reset while downloading {att}");
        }
        std::fs::create_dir_all(dest.parent().unwrap())?;
        std::fs::write(dest, format!("video {att}"))?;
        Ok(dest.to_path_buf())
    }
    async fn reply(&self, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn health(&self) -> MailHealth {
        MailHealth { ok: true, detail: "fake".into() }
    }
}

fn mail(id: &str, subject: &str, body: &str) -> InboundMail {
    InboundMail {
        id: id.into(),
        internet_message_id: format!("<{id}@example.gr>"),
        from_address: "a.papadaki@example.gr".into(),
        subject: subject.into(),
        body_text: body.into(),
        ..Default::default()
    }
}

/// LLM off: these tests are about the watcher, and must not reach a real
/// Ollama that happens to be running on the machine.
fn test_config() -> AppConfig {
    let mut c = AppConfig::default();
    c.llm.mode = omni_core::config::LlmMode::Off;
    c
}

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    repo.save_journalist("PAPADAKI", "Anna Papadaki", &["a.papadaki@example.gr".into()], 5)
        .unwrap();
    (dir, repo)
}

/// Break the queue's event table so every `enqueue` fails inside its
/// transaction — the shape of a real mid-processing database failure.
fn break_queue(dir: &tempfile::TempDir) {
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    conn.execute_batch("DROP TABLE job_events;").unwrap();
}

const BODY: &str = "1. ΠΑΡΕΛΑΣΗ ΣΤΟ ΗΡΑΚΛΕΙΟ\nhttps://www.youtube.com/watch?v=w0001\n\n2. ΚΑΥΣΩΝΑΣ\nhttps://we.tl/t-w0002";

#[tokio::test]
async fn a_message_that_fails_processing_is_not_marked_until_retries_run_out() {
    let (dir, repo) = repo();
    break_queue(&dir);
    let source = FakeSource::with(vec![mail("m1", "ΘΕΜΑΤΑ", BODY)]);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());

    for attempt in 1..MAX_PROCESS_ATTEMPTS {
        let r = watcher.poll_once().await.unwrap();
        assert_eq!(r.retrying, 1, "attempt {attempt}");
        assert!(source.marks().is_empty(), "marked after failed attempt {attempt}: {:?}", source.marks());
    }

    let r = watcher.poll_once().await.unwrap();
    assert_eq!(r.failed, 1);
    assert_eq!(source.marks(), vec![("m1".to_string(), MailOutcome::Failed)]);
    assert_eq!(repo.get_processed_mail("<m1@example.gr>").unwrap().unwrap().outcome, "FAILED");

    // Given up on and shown again (someone marked it unread): left alone for
    // the human, not fetched, not retried in a loop, not marked processed.
    source.redeliver(mail("m1", "ΘΕΜΑΤΑ", BODY), "m1-again");
    let r = watcher.poll_once().await.unwrap();
    assert_eq!((r.fetched, r.processed, r.failed, r.retrying), (0, 0, 0, 0));
    assert_eq!(r.already_seen, 2, "{r:?}");
    assert!(!source.fetches().contains(&"m1-again".to_string()));
    assert_eq!(source.marks().len(), 1);
}

#[tokio::test]
async fn a_parsed_message_queues_its_jobs_then_is_marked() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![mail("m1", "ΘΕΜΑΤΑ", BODY)]);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());

    let r = watcher.poll_once().await.unwrap();
    assert_eq!(r.processed, 1);
    assert_eq!(source.marks(), vec![("m1".to_string(), MailOutcome::Processed)]);

    let mut jobs = repo.get_all_jobs().unwrap();
    jobs.sort_by_key(|j| j.id);
    let slugs: Vec<&str> = jobs.iter().map(|j| j.slug.as_str()).collect();
    assert_eq!(slugs, vec!["1_PAPADAKI_PARELASIIRAKLEIO", "2_PAPADAKI_KAFSONAS"]);
    assert_eq!(jobs[0].status, JobStatus::Pending);
    assert_eq!(jobs[1].status, JobStatus::ManualDownload);
    assert_eq!(jobs[1].extraction_method.as_deref(), Some("locker"));
    assert_eq!(jobs[0].email_message_id.as_deref(), Some("<m1@example.gr>"));
    assert_eq!(jobs[0].email_source.as_deref(), Some("a.papadaki@example.gr"));
    assert_eq!(jobs[0].priority, 5, "journalist default priority");

    let row = repo.get_processed_mail("<m1@example.gr>").unwrap().unwrap();
    assert_eq!(row.outcome, "JOBS");
    let recorded: serde_json::Value = serde_json::from_str(&row.jobs_json).unwrap();
    assert_eq!(recorded.as_array().unwrap().len(), 2);
    assert_eq!(recorded[0]["result"]["outcome"], "created");
}

/// E-07: the same message seen again must not queue again — even when URL
/// dedup would let it through because the first job is no longer active.
#[tokio::test]
async fn a_redelivered_message_is_not_queued_twice() {
    let (_dir, repo) = repo();
    let first = mail("m1", "ΘΕΜΑΤΑ", BODY);
    let source = FakeSource::with(vec![first.clone()]);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();
    assert_eq!(repo.get_all_jobs().unwrap().len(), 2);

    // MCR cancels the video job, then the mail shows up unread again.
    let video = repo.get_all_jobs().unwrap().into_iter().find(|j| j.status == JobStatus::Pending).unwrap();
    repo.update_job_status(video.id, JobStatus::Cancelled, None, None, None).unwrap();
    source.redeliver(first, "m1-again");

    let r = watcher.poll_once().await.unwrap();
    assert_eq!(r.processed, 0);
    assert_eq!(repo.get_all_jobs().unwrap().len(), 2, "re-delivery queued the mail again");
    // Recognised from the listing alone: not even fetched.
    assert!(!source.fetches().contains(&"m1-again".to_string()), "{:?}", source.fetches());
}

#[tokio::test]
async fn an_urgent_subject_raises_priority_above_the_journalist_default() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![mail("m1", "ΕΚΤΑΚΤΟ: σεισμός", "https://youtu.be/w0003")]);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();
    let jobs = repo.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].priority, 5 + URGENT_PRIORITY_BOOST);
}

#[tokio::test]
async fn a_message_with_no_links_is_still_marked_processed() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![mail("m1", "Καλημέρα", "Θα στείλω τα λινκ αργότερα.")]);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();
    assert!(repo.get_all_jobs().unwrap().is_empty());
    assert_eq!(source.marks(), vec![("m1".to_string(), MailOutcome::Processed)]);
    assert_eq!(repo.get_processed_mail("<m1@example.gr>").unwrap().unwrap().outcome, "NO_LINKS");
}
fn mail_with_video(id: &str) -> InboundMail {
    let mut m = mail(id, "Βίντεο από το λιμάνι", "");
    m.attachments = vec![AttachmentMeta {
        id: "att/1+x=".into(), // Graph ids carry '/', '+' and '='
        name: "limani_kataplous.MP4".into(),
        content_type: "video/mp4".into(),
        size: 11,
    }];
    m
}

#[tokio::test]
async fn an_attached_video_is_saved_and_its_job_released_to_the_workers() {
    let (dir, repo) = repo();
    let source = FakeSource::with(vec![mail_with_video("m1")]);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone())
        .with_attachments_dir(dir.path().join("attachments"));
    watcher.poll_once().await.unwrap();

    let jobs = repo.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.status, JobStatus::Pending, "a saved attachment must be leasable");
    assert_eq!(job.extraction_method.as_deref(), Some("attachment"));
    assert_eq!(job.url, "attachment://m1@example.gr/att/1+x=");
    assert_eq!(job.slug, "1_PAPADAKI_LIMANIKATAPLOUS");
    let saved = PathBuf::from(job.source_path.clone().expect("source_path recorded"));
    assert_eq!(saved, dir.path().join("attachments").join(job.id.to_string()).join("source.mp4"));
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "video att/1+x=");
    assert_eq!(source.marks(), vec![("m1".to_string(), MailOutcome::Processed)]);

    let row = repo.get_processed_mail("<m1@example.gr>").unwrap().unwrap();
    assert!(row.jobs_json.contains("\"status\":\"PENDING\""), "{}", row.jobs_json);
}

#[tokio::test]
async fn a_failed_attachment_download_leaves_the_job_for_mcr_and_the_mail_processed() {
    let (dir, repo) = repo();
    let source = FakeSource::with(vec![mail_with_video("m1")]);
    source.fail_downloads.store(true, std::sync::atomic::Ordering::SeqCst);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone())
        .with_attachments_dir(dir.path().join("attachments"));
    watcher.poll_once().await.unwrap();

    let jobs = repo.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, JobStatus::ManualDownload);
    assert!(jobs[0].source_path.is_none());
    let events = repo.get_job_events(jobs[0].id, 10).unwrap();
    assert!(events.iter().any(|e| e.message.contains("Could not save the attachment")), "{events:?}");
    // The job exists, so retrying the mail would only duplicate it.
    assert_eq!(source.marks(), vec![("m1".to_string(), MailOutcome::Processed)]);
}
// ---------------------------------------------------------------------------
// Read-only mailbox (plan P4.8): the station's Graph app has Mail.Read only.
// ---------------------------------------------------------------------------

fn numbered(n: usize) -> InboundMail {
    mail(&format!("r{n}"), "ΘΕΜΑ", &format!("1. ΤΕΣΤ\nhttps://www.youtube.com/watch?v=ro{n:04}"))
}

/// The P4.2 fetch asked for `isRead eq false`, oldest first, a page at a
/// time. With Mail.Read nothing is ever marked read, so once a page of
/// processed-but-unread mail piled up, every poll saw only that page and new
/// mail was never ingested.
#[tokio::test]
async fn processed_mail_that_stays_unread_does_not_stall_new_mail() {
    let (_dir, repo) = repo();
    let source = FakeSource::with((0..60).map(numbered).collect());
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());

    let mut processed = 0;
    for _ in 0..4 {
        processed += watcher.poll_once().await.unwrap().processed;
    }
    assert_eq!(processed, 60);
    assert_eq!(repo.get_all_jobs().unwrap().len(), 60);

    // Nothing was marked on the server, and a new mail still gets through.
    source.arrive(numbered(60));
    let r = watcher.poll_once().await.unwrap();
    assert_eq!((r.fetched, r.processed), (1, 1), "{r:?}");
    assert_eq!(repo.get_all_jobs().unwrap().len(), 61);

    // A poll lists from just before the checkpoint, not from the beginning.
    let last_since = *source.listings.lock().unwrap().last().unwrap();
    assert!(last_since > Utc::now() - Duration::minutes(75), "listing restarted from {last_since}");
    // Each mail was fetched once in all of that.
    assert_eq!(source.fetches().len(), 61);
}

#[tokio::test]
async fn a_restart_with_the_checkpoint_lost_queues_nothing_twice() {
    let (dir, repo) = repo();
    let source = FakeSource::with((0..5).map(numbered).collect());
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();
    assert_eq!(repo.get_all_jobs().unwrap().len(), 5);

    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    conn.execute_batch("DELETE FROM mail_checkpoints;").unwrap();
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    let r = watcher.poll_once().await.unwrap();
    assert_eq!((r.already_seen, r.fetched), (5, 0), "{r:?}");
    assert_eq!(repo.get_all_jobs().unwrap().len(), 5);
}

#[tokio::test]
async fn the_first_poll_looks_back_a_day_and_no_further() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![]);
    source.arrive_at(numbered(1), Utc::now() - FIRST_RUN_LOOKBACK - Duration::hours(6));
    source.arrive_at(numbered(2), Utc::now() - Duration::hours(2));
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());

    let r = watcher.poll_once().await.unwrap();
    assert_eq!((r.listed, r.processed), (1, 1), "{r:?}");
    assert!(repo.get_processed_mail("<r1@example.gr>").unwrap().is_none());
    assert!(repo.get_processed_mail("<r2@example.gr>").unwrap().is_some());
}

/// A journalist's mail lands in Junk on Friday; on Monday someone moves it
/// to the Inbox. Its received time is days old; its change time is now.
#[tokio::test]
async fn a_mail_rescued_from_junk_days_later_is_ingested() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![numbered(1)]);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();

    let mut rescued = numbered(2);
    rescued.received_at = Some(Utc::now() - Duration::days(2));
    source.arrive(rescued);
    let r = watcher.poll_once().await.unwrap();
    assert_eq!(r.processed, 1, "{r:?}");
    assert!(repo.get_processed_mail("<r2@example.gr>").unwrap().is_some());
}

/// Someone flags a month-old mail: it shows as changed, but it is not news.
#[tokio::test]
async fn an_old_mail_that_is_only_touched_is_not_ingested() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![numbered(1)]);
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();

    let mut old = numbered(2);
    old.received_at = Some(Utc::now() - Duration::days(30));
    source.arrive(old);
    let r = watcher.poll_once().await.unwrap();
    assert_eq!((r.too_old, r.fetched), (1, 0), "{r:?}");
    assert_eq!(repo.get_all_jobs().unwrap().len(), 1);
}

#[tokio::test]
async fn a_mail_still_being_retried_holds_the_checkpoint_but_not_the_mail_after_it() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![numbered(1), numbered(2)]);
    source.fail_fetch.lock().unwrap().insert("r1".into());
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    let key = source.checkpoint_key();

    let r = watcher.poll_once().await.unwrap();
    assert_eq!((r.retrying, r.processed), (1, 1), "{r:?}");
    assert_eq!(repo.get_mail_checkpoint(&key).unwrap(), None, "the checkpoint passed a mail still being retried");

    for _ in 1..MAX_PROCESS_ATTEMPTS {
        watcher.poll_once().await.unwrap();
    }
    assert_eq!(repo.get_processed_mail("<r1@example.gr>").unwrap().unwrap().outcome, "FAILED");
    let r2_changed = source.inbox.lock().unwrap()[1].modified_at;
    assert_eq!(repo.get_mail_checkpoint(&key).unwrap(), Some(r2_changed));
    // r2 was processed once, on the first poll, and only fetched then.
    assert_eq!(source.fetches().iter().filter(|id| *id == "r2").count(), 1);
    assert_eq!(repo.get_all_jobs().unwrap().len(), 1);
}

#[tokio::test]
async fn a_mail_deleted_between_listing_and_fetch_is_skipped_not_retried() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![numbered(1)]);
    source.vanished.lock().unwrap().insert("r1".into());
    let watcher = EmailWatcher::with_source(test_config(), repo.clone(), source.clone());
    let r = watcher.poll_once().await.unwrap();
    assert_eq!((r.retrying, r.failed, r.processed), (0, 0, 0), "{r:?}");
    assert!(repo.get_mail_checkpoint(&source.checkpoint_key()).unwrap().is_some());
}
