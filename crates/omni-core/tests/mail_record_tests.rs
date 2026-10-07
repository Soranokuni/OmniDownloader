//! What the database keeps about each handled mail for the MCR mail view
//! (plan P7.6): its text and headers, how long the text stays, and which job
//! an article's extra videos came from.

use chrono::{Duration, Utc};
use omni_core::models::{NewJob, ProcessedMail};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

fn handled(key: &str, received_days_ago: i64) -> ProcessedMail {
    ProcessedMail {
        internet_message_id: key.into(),
        source_id: Some("AAMk-1".into()),
        outcome: "JOBS".into(),
        from_address: Some("e.georgiou@example.gr".into()),
        subject: Some("ΘΕΜΑΤΑ ΕΛΕΝΗΣ".into()),
        jobs_json: "[]".into(),
        received_at: Some(Utc::now() - Duration::days(received_days_ago)),
        from_name: Some("Ελένη Γεωργίου".into()),
        to: vec!["ingest@example.gr".into()],
        cc: vec!["master@example.gr".into(), "desk@example.gr".into()],
        body_text: Some("1. ΣΕΙΣΜΟΣ\nhttps://www.youtube.com/watch?v=abc".into()),
        attachments_json: Some(r#"[{"id":"2","name":"clip.mp4","content_type":"video/mp4","size":10}]"#.into()),
        parse_json: Some(r#"{"journalist":"GEORGIOU","how":"sender","outcome":"JOBS"}"#.into()),
        ..Default::default()
    }
}

#[test]
fn a_handled_mail_keeps_its_text_and_headers() {
    let (_d, repo) = repo();
    repo.record_processed_mail(&handled("<m1@example.gr>", 0)).unwrap();

    let back = repo.get_processed_mail("<m1@example.gr>").unwrap().unwrap();
    assert_eq!(back.from_name.as_deref(), Some("Ελένη Γεωργίου"));
    assert_eq!(back.to, vec!["ingest@example.gr"]);
    assert_eq!(back.cc, vec!["master@example.gr", "desk@example.gr"]);
    assert!(back.body_text.as_deref().unwrap().contains("youtube.com/watch?v=abc"));
    assert!(back.attachments_json.as_deref().unwrap().contains("clip.mp4"));
    assert!(back.parse_json.as_deref().unwrap().contains("GEORGIOU"));
    assert!(back.received_at.is_some());
    // The admin history reads the same rows.
    let listed = repo.list_processed_mail(10).unwrap();
    assert_eq!(listed[0].body_text, back.body_text);
}

/// A later record that knows only the headers (the watcher giving up on a
/// re-read) must not erase what an earlier one stored.
#[test]
fn a_record_without_text_does_not_erase_the_text() {
    let (_d, repo) = repo();
    repo.record_processed_mail(&handled("<m1@example.gr>", 0)).unwrap();
    repo.record_processed_mail(&ProcessedMail {
        internet_message_id: "<m1@example.gr>".into(),
        outcome: "FAILED".into(),
        ..Default::default()
    })
    .unwrap();

    let back = repo.get_processed_mail("<m1@example.gr>").unwrap().unwrap();
    assert_eq!(back.outcome, "FAILED");
    assert!(back.body_text.is_some(), "the stored text was erased");
    assert_eq!(back.cc.len(), 2);
}

#[test]
fn old_mail_loses_its_text_but_keeps_its_record() {
    let (_d, repo) = repo();
    repo.record_processed_mail(&handled("<old@example.gr>", 45)).unwrap();
    repo.record_processed_mail(&handled("<new@example.gr>", 3)).unwrap();

    assert_eq!(repo.purge_mail_text(30).unwrap(), 1);
    let old = repo.get_processed_mail("<old@example.gr>").unwrap().unwrap();
    assert!(old.body_text.is_none());
    assert_eq!(old.subject.as_deref(), Some("ΘΕΜΑΤΑ ΕΛΕΝΗΣ"), "the record itself stays");
    assert!(old.parse_json.is_some());
    assert!(repo.get_processed_mail("<new@example.gr>").unwrap().unwrap().body_text.is_some());

    // Nothing left to clear.
    assert_eq!(repo.purge_mail_text(30).unwrap(), 0);
}

#[test]
fn a_video_found_in_an_article_points_at_the_article_job() {
    let (_d, repo) = repo();
    let parent = repo
        .enqueue(&NewJob::new("https://portal.gr/a", "1A_PAPADAKI_X", "PAPADAKI"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    let mut sibling = NewJob::new("https://x.com/i/status/2", "1B_PAPADAKI_X", "PAPADAKI");
    sibling.parent_job_id = Some(parent);
    let child = repo.enqueue(&sibling, DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();

    assert_eq!(repo.get_job(child).unwrap().unwrap().parent_job_id, Some(parent));
    assert_eq!(repo.get_job(parent).unwrap().unwrap().parent_job_id, None);
}
