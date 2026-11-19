//! P11.11: remembering a sender for a journalist (a), and the one automatic
//! reading of a mail the watcher gave up on (b).

use chrono::{Duration, Utc};

use omni_core::models::ProcessedMail;
use omni_core::repository::{AddressAdded, Repository, MAX_JOURNALIST_EMAILS};

fn repo() -> (Repository, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (repo, dir)
}

#[test]
fn a_sender_is_added_once_and_never_taken_from_another_journalist() {
    let (repo, _dir) = repo();
    repo.save_journalist("PAPADAKI", "Anna Papadaki", &["a.papadaki@example.gr".into()], 0).unwrap();
    repo.save_journalist("NIKOLAOU", "Giorgos Nikolaou", &[], 0).unwrap();

    assert_eq!(repo.add_journalist_email("nikolaou", " Crew.One@Example.NET ").unwrap(), AddressAdded::Added);
    assert_eq!(repo.find_journalist_by_email("crew.one@example.net").unwrap().as_deref(), Some("NIKOLAOU"));
    assert_eq!(repo.add_journalist_email("NIKOLAOU", "crew.one@example.net").unwrap(), AddressAdded::AlreadyTheirs);

    // Someone else's address, in any case: refused, and it stays where it was.
    assert_eq!(
        repo.add_journalist_email("NIKOLAOU", "A.Papadaki@example.gr").unwrap(),
        AddressAdded::TakenBy("PAPADAKI".into())
    );
    assert_eq!(repo.find_journalist_by_email("a.papadaki@example.gr").unwrap().as_deref(), Some("PAPADAKI"));
    let nikolaou = repo.list_journalists().unwrap().into_iter().find(|j| j.surname == "NIKOLAOU").unwrap();
    assert_eq!(nikolaou.emails, vec!["crew.one@example.net"]);

    assert_eq!(repo.add_journalist_email("NOBODY", "x@example.net").unwrap(), AddressAdded::NoSuchJournalist);
    assert!(repo.add_journalist_email("NIKOLAOU", "not-an-address").is_err());

    let full: Vec<String> = (0..MAX_JOURNALIST_EMAILS).map(|i| format!("p{i}@example.org")).collect();
    repo.save_journalist("FULL", "Full Person", &full, 0).unwrap();
    assert_eq!(repo.add_journalist_email("FULL", "one.more@example.org").unwrap(), AddressAdded::Full);
}

fn failed(repo: &Repository, key: &str, received_ago: Duration, source: Option<&str>) {
    repo.record_processed_mail(&ProcessedMail {
        internet_message_id: key.into(),
        source_id: source.map(String::from),
        outcome: "FAILED".into(),
        subject: Some(format!("subject {key}")),
        received_at: Some(Utc::now() - received_ago),
        ..Default::default()
    })
    .unwrap();
}

fn pending(repo: &Repository) -> Vec<String> {
    let mut keys: Vec<String> = repo.pending_mail_reprocess().unwrap().into_iter().map(|(k, _, _)| k).collect();
    keys.sort();
    keys
}

#[test]
fn a_failed_mail_is_read_again_automatically_once_and_only_when_young() {
    let (repo, _dir) = repo();
    let since = Utc::now() - Duration::hours(2);
    failed(&repo, "<young@example.gr>", Duration::minutes(30), Some("AAMk-young"));
    failed(&repo, "<old@example.gr>", Duration::hours(3), Some("AAMk-old"));
    failed(&repo, "<noid@example.gr>", Duration::minutes(10), None);
    // A person already asked for this one: theirs, not the system's.
    failed(&repo, "<asked@example.gr>", Duration::minutes(10), Some("AAMk-asked"));
    repo.request_mail_reprocess("<asked@example.gr>", "mcr").unwrap();
    // Read fine: nothing to do.
    repo.record_processed_mail(&ProcessedMail {
        internet_message_id: "<fine@example.gr>".into(),
        source_id: Some("AAMk-fine".into()),
        outcome: "JOBS".into(),
        received_at: Some(Utc::now()),
        ..Default::default()
    })
    .unwrap();

    let asked = repo.request_failed_mail_auto_retry(since, "restart").unwrap();
    assert_eq!(asked, vec![("<young@example.gr>".to_string(), Some("subject <young@example.gr>".to_string()))]);
    assert_eq!(pending(&repo), vec!["<asked@example.gr>", "<young@example.gr>"]);
    let (_, by, _) = repo.pending_mail_reprocess().unwrap().into_iter().find(|(k, _, _)| k == "<young@example.gr>").unwrap();
    assert_eq!(by, "AAMk-young", "fetched by its mailbox id");
    let (at, cause) = repo.mail_auto_retry("<young@example.gr>").unwrap().expect("recorded");
    assert!(at.is_some());
    assert_eq!(cause, "restart");
    assert!(repo.mail_auto_retry("<old@example.gr>").unwrap().is_none());

    // The reading fails again and the mail is FAILED as before: no second chance,
    // whatever sets it off.
    repo.finish_mail_reprocess("<young@example.gr>").unwrap();
    failed(&repo, "<young@example.gr>", Duration::minutes(31), Some("AAMk-young"));
    assert!(repo.request_failed_mail_auto_retry(since, "tool update").unwrap().is_empty());
    assert_eq!(pending(&repo), vec!["<asked@example.gr>"]);

    // The person's reading ends and the mail is still FAILED: it stays theirs.
    repo.finish_mail_reprocess("<asked@example.gr>").unwrap();
    assert!(repo.request_failed_mail_auto_retry(since, "restart").unwrap().is_empty());
    assert!(pending(&repo).is_empty());
    let (_, cause) = repo.mail_auto_retry("<asked@example.gr>").unwrap().unwrap();
    assert_eq!(cause, omni_core::repository::MAIL_RETRY_BY_A_PERSON);
}

/// Review round 2: a failed mail that made a job before it failed is not re-read by the
/// system, whatever became of that job (here MCR cancelled it).
#[test]
fn a_failed_mail_that_made_a_job_is_left_to_a_person() {
    use omni_core::models::NewJob;
    let (repo, _dir) = repo();
    failed(&repo, "<partial@example.gr>", Duration::minutes(10), Some("AAMk-partial"));
    let mut job = NewJob::new("https://www.youtube.com/watch?v=partial0001", "1_MCR_X", "MCR");
    job.email_message_id = Some("<partial@example.gr>".into());
    let id = repo.enqueue(&job, 24).unwrap().job_id();
    assert!(matches!(repo.cancel_job(id).unwrap(), omni_core::repository::CancelOutcome::Cancelled), "MCR cancels it");

    assert!(repo.request_failed_mail_auto_retry(Utc::now() - Duration::hours(2), "restart").unwrap().is_empty());
    assert!(pending(&repo).is_empty());
    assert!(repo.mail_auto_retry("<partial@example.gr>").unwrap().is_none());
}

/// Review round 2: the system's own re-read treats a link delivered under any journalist
/// as delivered; the normal path still lets another journalist have their own file.
#[test]
fn an_automatic_enqueue_never_delivers_a_link_delivered_under_another_name() {
    use omni_core::models::{Enqueued, JobStatus, NewJob};
    let (repo, _dir) = repo();
    let url = "https://www.youtube.com/watch?v=handqueued01";
    let id = repo.enqueue(&NewJob::new(url, "1_NIKOLAOU_LIMANI", "NIKOLAOU"), 24).unwrap().job_id();
    assert!(repo.lease_job_by_id(id, "host:1:0", 180).unwrap());
    assert!(repo.finish(id, "host:1:0", JobStatus::Completed, None, None, None).unwrap());

    let again = NewJob::new(url, "1_MCR_LIMANI", "MCR");
    assert_eq!(repo.enqueue_automatic(&again, 24).unwrap(), Enqueued::DuplicateRecent { existing_id: id });
    assert!(matches!(repo.enqueue(&again, 24).unwrap(), Enqueued::Created { .. }), "the normal path is unchanged");
}
