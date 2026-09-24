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
use omni_core::repository::Repository;
use omni_email::mail::InboundMail;
use omni_email::source::{MailHealth, MailOutcome, MailSource};
use omni_email::watcher::{EmailWatcher, MAX_PROCESS_ATTEMPTS};

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

fn mail(id: &str, body: &str) -> InboundMail {
    InboundMail {
        id: id.into(),
        internet_message_id: format!("<{id}@example.gr>"),
        from_address: "a.papadaki@example.gr".into(),
        subject: "ΘΕΜΑΤΑ".into(),
        body_text: body.into(),
        ..Default::default()
    }
}

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

/// A config whose LLM endpoint refuses connections at once, so processing
/// fails the way it did whenever Ollama was down.
fn config_with_dead_llm() -> AppConfig {
    AppConfig {
        ollama_endpoint: "http://127.0.0.1:9/v1".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_message_that_fails_processing_is_not_marked_until_retries_run_out() {
    let (_dir, repo) = repo();
    let source = FakeSource::with(vec![mail("m1", "1. ΘΕΜΑ\nhttps://youtu.be/abc")]);
    let watcher = EmailWatcher::with_source(config_with_dead_llm(), repo, source.clone());

    for attempt in 1..MAX_PROCESS_ATTEMPTS {
        let r = watcher.poll_once().await.unwrap();
        assert_eq!(r.retrying, 1, "attempt {attempt}");
        assert!(source.marks().is_empty(), "marked after failed attempt {attempt}: {:?}", source.marks());
    }

    let r = watcher.poll_once().await.unwrap();
    assert_eq!(r.failed, 1);
    assert_eq!(source.marks(), vec![("m1".to_string(), MailOutcome::Failed)]);

    // Given up on: the next poll does not see it again.
    assert_eq!(watcher.poll_once().await.unwrap().fetched, 0);
}
