//! The watcher against a scripted mail source (plan P4.1).
//!
//! Regression for E-02: the old IMAP loop marked every message `\Seen` after
//! the processing loop, whether or not processing worked, so a failure lost
//! the journalist's links without trace.

use anyhow::{bail, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use omni_core::config::AppConfig;
use omni_core::models::JobStatus;
use omni_core::repository::Repository;
use omni_email::mail::InboundMail;
use omni_email::source::{MailHealth, MailOutcome, MailSource};
use omni_email::watcher::{EmailWatcher, MAX_PROCESS_ATTEMPTS, URGENT_PRIORITY_BOOST};

#[derive(Default)]
struct FakeSource {
    inbox: Mutex<Vec<InboundMail>>,
    marks: Mutex<Vec<(String, MailOutcome)>>,
}

impl FakeSource {
    fn with(mails: Vec<InboundMail>) -> Arc<Self> {
        Arc::new(Self {
            inbox: Mutex::new(mails),
            marks: Mutex::default(),
        })
    }
    fn marks(&self) -> Vec<(String, MailOutcome)> {
        self.marks.lock().unwrap().clone()
    }
    /// The server shows a message again under a new provider id (a mail
    /// dragged back to the inbox, a Graph move that failed after queueing).
    fn redeliver(&self, mut mail: InboundMail, new_id: &str) {
        mail.id = new_id.into();
        self.inbox.lock().unwrap().push(mail);
    }
}

#[async_trait]
impl MailSource for FakeSource {
    fn describe(&self) -> String {
        "fake".into()
    }
    fn is_configured(&self) -> bool {
        true
    }
    async fn fetch_unprocessed(&self, limit: usize) -> Result<Vec<InboundMail>> {
        // Like a server: whatever has not been marked is still unread.
        let marked: Vec<String> = self.marks().into_iter().map(|(id, _)| id).collect();
        Ok(self
            .inbox
            .lock()
            .unwrap()
            .iter()
            .filter(|m| !marked.contains(&m.id))
            .take(limit)
            .cloned()
            .collect())
    }
    async fn mark_processed(&self, mail_id: &str, outcome: MailOutcome) -> Result<()> {
        self.marks.lock().unwrap().push((mail_id.to_string(), outcome));
        Ok(())
    }
    async fn download_attachment(&self, _: &str, _: &str, _: &Path) -> Result<PathBuf> {
        bail!("not in this test")
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
    let watcher = EmailWatcher::with_source(AppConfig::default(), repo.clone(), source.clone());

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
    // the human, not retried in a loop and not marked processed.
    source.redeliver(mail("m1", "ΘΕΜΑΤΑ", BODY), "m1-again");
    let r = watcher.poll_once().await.unwrap();
    assert_eq!((r.fetched, r.processed, r.failed, r.retrying), (1, 0, 0, 0));
    assert_eq!(source.marks().len(), 1);
}

#[tokio::test]
async fn a_parsed_message_queues_its_jobs_then_is_marked() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![mail("m1", "ΘΕΜΑΤΑ", BODY)]);
    let watcher = EmailWatcher::with_source(AppConfig::default(), repo.clone(), source.clone());

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
    let watcher = EmailWatcher::with_source(AppConfig::default(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();
    assert_eq!(repo.get_all_jobs().unwrap().len(), 2);

    // MCR cancels the video job, then the mail shows up unread again.
    let video = repo.get_all_jobs().unwrap().into_iter().find(|j| j.status == JobStatus::Pending).unwrap();
    repo.update_job_status(video.id, JobStatus::Cancelled, None, None, None).unwrap();
    source.redeliver(first, "m1-again");

    let r = watcher.poll_once().await.unwrap();
    assert_eq!(r.processed, 1);
    assert_eq!(repo.get_all_jobs().unwrap().len(), 2, "re-delivery queued the mail again");
    // Marked on the server too, so it stops showing as unread.
    assert!(source.marks().contains(&("m1-again".to_string(), MailOutcome::Processed)));
}

#[tokio::test]
async fn an_urgent_subject_raises_priority_above_the_journalist_default() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![mail("m1", "ΕΚΤΑΚΤΟ: σεισμός", "https://youtu.be/w0003")]);
    let watcher = EmailWatcher::with_source(AppConfig::default(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();
    let jobs = repo.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].priority, 5 + URGENT_PRIORITY_BOOST);
}

#[tokio::test]
async fn a_message_with_no_links_is_still_marked_processed() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![mail("m1", "Καλημέρα", "Θα στείλω τα λινκ αργότερα.")]);
    let watcher = EmailWatcher::with_source(AppConfig::default(), repo.clone(), source.clone());
    watcher.poll_once().await.unwrap();
    assert!(repo.get_all_jobs().unwrap().is_empty());
    assert_eq!(source.marks(), vec![("m1".to_string(), MailOutcome::Processed)]);
    assert_eq!(repo.get_processed_mail("<m1@example.gr>").unwrap().unwrap().outcome, "NO_LINKS");
}