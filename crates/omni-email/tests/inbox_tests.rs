//! The MCR mail view's list (plan P7.7): entries, their state, the filter
//! chips, the search and the pages, against a real database.
//!
//! All names and addresses are invented (the repository is public).

use chrono::{Duration, Utc};

use omni_core::models::{JobStatus, NewJob, ProcessedMail};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use omni_email::inbox::{self, EntryKind, EntryState, InboxFilter};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

/// A handled mail `minutes_ago`, with one job per `(url, status)`.
fn mail(repo: &Repository, key: &str, subject: &str, minutes_ago: i64, jobs: &[(&str, JobStatus)]) -> Vec<i64> {
    let mut ids = Vec::new();
    let mut records = Vec::new();
    for (i, (url, status)) in jobs.iter().enumerate() {
        let index = (i + 1).to_string();
        let slug = format!("{index}_GEORGIOU_{}", key.trim_matches(['<', '>']).replace('@', "-").to_uppercase());
        let mut new = NewJob::new(*url, slug.clone(), "GEORGIOU");
        new.index_str = index.clone();
        new.email_message_id = Some(key.into());
        let result = repo.enqueue(&new, DEFAULT_DEDUP_WINDOW_HOURS).unwrap();
        let id = result.job_id();
        set_status(repo, id, *status);
        records.push(serde_json::json!({
            "index_str": index, "slug": slug, "url": url,
            "status": "PENDING", "result": result,
        }));
        ids.push(id);
    }
    repo.record_processed_mail(&ProcessedMail {
        internet_message_id: key.into(),
        outcome: if jobs.is_empty() { "PHOTOS_ONLY".into() } else { "JOBS".into() },
        from_address: Some("e.georgiou@example.gr".into()),
        from_name: Some("Ελένη Γεωργίου".into()),
        subject: Some(subject.into()),
        jobs_json: serde_json::Value::Array(records).to_string(),
        received_at: Some(Utc::now() - Duration::minutes(minutes_ago)),
        body_text: Some(format!("Καλησπέρα,\n1. {subject}\n{}", jobs.first().map(|j| j.0).unwrap_or(""))),
        parse_json: Some(r#"{"journalist":"GEORGIOU","how":"sender","outcome":"JOBS","urgent":true}"#.into()),
        ..Default::default()
    })
    .unwrap();
    ids
}

fn set_status(repo: &Repository, id: i64, status: JobStatus) {
    if status != JobStatus::Pending {
        repo.update_job_status(id, status, None, None, None).unwrap();
    }
}

fn list(repo: &Repository) -> Vec<inbox::Entry> {
    let since = Utc::now() - Duration::days(30);
    let mails = repo.inbox_mail_rows(since).unwrap();
    let extra = inbox::referenced_job_ids(&mails);
    let jobs = repo.inbox_job_rows(since, &extra).unwrap();
    inbox::entries(mails, jobs, since)
}

#[test]
fn each_mail_is_one_entry_with_the_state_of_its_videos() {
    let (_d, repo) = repo();
    mail(&repo, "<done@x>", "ΚΑΙΡΟΣ", 50, &[("https://youtu.be/d1", JobStatus::Completed)]);
    mail(&repo, "<busy@x>", "ΣΕΙΣΜΟΣ", 40, &[("https://youtu.be/b1", JobStatus::Completed), ("https://youtu.be/b2", JobStatus::Pending)]);
    mail(&repo, "<bad@x>", "ΠΥΡΚΑΓΙΑ", 30, &[("https://youtu.be/x1", JobStatus::RequiresReview)]);
    mail(&repo, "<none@x>", "ΔΕΛΤΙΟ ΤΥΠΟΥ", 20, &[]);

    let entries = list(&repo);
    let states: Vec<(&str, EntryState)> = entries.iter().map(|e| (e.key.as_str(), e.state)).collect();
    assert_eq!(
        states,
        vec![
            ("<none@x>", EntryState::Empty),
            ("<bad@x>", EntryState::Attention),
            ("<busy@x>", EntryState::Active),
            ("<done@x>", EntryState::Done),
        ],
        "newest first, each with the state of its videos"
    );
    let busy = entries.iter().find(|e| e.key == "<busy@x>").unwrap();
    assert_eq!(busy.jobs.len(), 2);
    assert_eq!(busy.journalist.as_deref(), Some("GEORGIOU"));
    assert_eq!(busy.how.as_deref(), Some("από τη διεύθυνση του αποστολέα"));
    assert!(busy.urgent);
    assert_eq!(busy.preview, "Καλησπέρα, 1. ΣΕΙΣΜΟΣ youtu.be/b1");

    let page = inbox::page(entries, InboxFilter::All, "", 1, 20);
    assert_eq!((page.counts.all, page.counts.active, page.counts.attention, page.counts.done, page.counts.empty), (4, 1, 1, 1, 1));
}

#[test]
fn a_link_added_by_hand_is_an_entry_with_its_article_videos() {
    let (_d, repo) = repo();
    let root = repo
        .enqueue(&NewJob::new("https://www.ertnews.gr/video/kairos/", "1_MCR_KAIROS", "MCR"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    let mut child = NewJob::new("https://x.com/i/status/9", "1B_MCR_KAIROS", "MCR");
    child.parent_job_id = Some(root);
    child.index_str = "1B".into();
    repo.enqueue(&child, DEFAULT_DEDUP_WINDOW_HOURS).unwrap();

    let entries = list(&repo);
    assert_eq!(entries.len(), 1, "the article's video is not an entry of its own");
    let e = &entries[0];
    assert_eq!((e.kind, e.key.as_str()), (EntryKind::Manual, root.to_string().as_str()));
    assert_eq!(e.jobs.len(), 2);
    assert_eq!(e.subject, "ertnews.gr/video/kairos/");
    assert_eq!(e.state, EntryState::Active);
}

#[test]
fn a_mail_that_could_not_be_read_needs_attention() {
    let (_d, repo) = repo();
    repo.record_processed_mail(&ProcessedMail {
        internet_message_id: "<failed@x>".into(),
        outcome: "FAILED".into(),
        subject: Some("Πλάνα λιμάνι".into()),
        received_at: Some(Utc::now()),
        ..Default::default()
    })
    .unwrap();
    let entries = list(&repo);
    assert_eq!(entries[0].state, EntryState::Failed);
    let page = inbox::page(entries, InboxFilter::Attention, "", 1, 20);
    assert_eq!(page.total, 1, "a failed mail is under «needs attention»");
}

#[test]
fn search_finds_greek_with_or_without_accents_and_in_latin() {
    let (_d, repo) = repo();
    mail(&repo, "<a@x>", "Σεισμός στο Ηράκλειο", 10, &[("https://youtu.be/s1", JobStatus::Pending)]);
    mail(&repo, "<b@x>", "Καιρός", 5, &[("https://youtu.be/k1", JobStatus::Pending)]);

    for q in ["σεισμος", "ΣΕΙΣΜΌΣ", "seismos", "youtu.be/s1", "1_GEORGIOU"] {
        let page = inbox::page(list(&repo), InboxFilter::All, q, 1, 20);
        let keys: Vec<&str> = page.entries.iter().map(|e| e.key.as_str()).collect();
        if q == "1_GEORGIOU" {
            assert_eq!(keys.len(), 2, "{q}: {keys:?}");
        } else {
            assert_eq!(keys, vec!["<a@x>"], "{q}");
        }
    }
    assert_eq!(inbox::page(list(&repo), InboxFilter::All, "ηφαίστειο", 1, 20).total, 0);
}

#[test]
fn filters_and_pages_cut_the_list_without_changing_the_counts() {
    let (_d, repo) = repo();
    for i in 0..12 {
        mail(&repo, &format!("<m{i:02}@x>"), "ΘΕΜΑ", 100 - i, &[(&format!("https://youtu.be/p{i}"), JobStatus::Pending)]);
    }
    mail(&repo, "<bad@x>", "ΘΕΜΑ", 1, &[("https://youtu.be/bad", JobStatus::Failed)]);

    let first = inbox::page(list(&repo), InboxFilter::Active, "", 1, 5);
    assert_eq!((first.total, first.entries.len(), first.page), (12, 5, 1));
    assert_eq!(first.entries[0].key, "<m11@x>");
    let last = inbox::page(list(&repo), InboxFilter::Active, "", 3, 5);
    assert_eq!(last.entries.len(), 2);
    let beyond = inbox::page(list(&repo), InboxFilter::Active, "", 99, 5);
    assert_eq!(beyond.page, 3, "a page past the end shows the last one");
    assert_eq!((first.counts.all, first.counts.active, first.counts.attention), (13, 12, 1));
}

#[test]
fn a_duplicate_link_shows_the_job_that_does_the_work_even_when_older_than_the_window() {
    let (_d, repo) = repo();
    let old = repo
        .enqueue(&NewJob::new("https://youtu.be/olddup", "1_MCR_OLD", "MCR"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    // Make the existing job older than the list's window.
    let conn = rusqlite::Connection::open(_d.path().join("omni.db")).unwrap();
    conn.execute("UPDATE queue SET created_at = '2020-01-01T00:00:00.000Z' WHERE id = ?", [old]).unwrap();
    mail(&repo, "<dup@x>", "ΘΕΜΑ", 5, &[("https://youtu.be/olddup", JobStatus::Pending)]);

    let entries = list(&repo);
    let dup = entries.iter().find(|e| e.key == "<dup@x>").unwrap();
    assert_eq!(dup.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), vec![old]);
    assert!(entries.iter().all(|e| e.kind == EntryKind::Mail), "the old job is not listed on its own");
}

/// Plan P7.11: opening a mail from a video lands on the page that holds it.
#[test]
fn the_page_that_holds_an_entry_is_found_by_its_id() {
    let (_d, repo) = repo();
    for i in 0..12 {
        mail(&repo, &format!("<f{i:02}@x>"), "ΘΕΜΑ", 100 - i, &[(&format!("https://youtu.be/f{i}"), JobStatus::Pending)]);
    }
    // Newest first: <f11@x> is on page 1, <f00@x> on page 3 of 5 per page.
    let p = inbox::page_with_focus(list(&repo), InboxFilter::All, "", 1, 5, Some("mail:<f00@x>"));
    assert_eq!(p.page, 3);
    assert!(p.entries.iter().any(|e| e.key == "<f00@x>"));
    assert_eq!(p.entries.iter().find(|e| e.key == "<f00@x>").unwrap().id(), "mail:<f00@x>");
    // Not in the list: the page asked for.
    let p = inbox::page_with_focus(list(&repo), InboxFilter::All, "", 2, 5, Some("mail:<gone@x>"));
    assert_eq!(p.page, 2);
}
