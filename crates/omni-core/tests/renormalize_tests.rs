//! P14.8b: the dedup keys stored before a change of `urlnorm::normalize` are
//! made again at the next start, so the same link sent before and after an
//! upgrade is still one job (the owner's rule: never the same video twice).
//! Real SQLite files; invented links.

use anyhow::Result;
use chrono::{Duration, Utc};
use rusqlite::params;
use tempfile::TempDir;

use omni_core::models::{Enqueued, JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

/// A Facebook video under the shape journalists paste, and another shape of it.
const SENT: &str = "https://www.facebook.com/NewsPage/videos/99/";
const AGAIN: &str = "https://www.facebook.com/watch/?v=99&mibextid=abc";
/// What the rules before P14.8 stored for `SENT`.
const OLD_KEY: &str = "https://www.facebook.com/NewsPage/videos/99";

fn queue(repo: &Repository, url: &str, n: u32) -> Enqueued {
    repo.enqueue(&NewJob::new(url, &format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS).unwrap()
}

/// The database as an older version left it: `ids` keep the old rule's key,
/// and no normalization version is stored.
fn as_before_the_upgrade(dir: &TempDir, ids: &[i64]) {
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    for id in ids {
        conn.execute("UPDATE queue SET url_normalized = ? WHERE id = ?", params![OLD_KEY, id]).unwrap();
    }
    conn.execute("DELETE FROM app_meta WHERE key = 'url_normalize_version'", []).unwrap();
}

fn key_of(dir: &TempDir, id: i64) -> String {
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    conn.query_row("SELECT url_normalized FROM queue WHERE id = ?", params![id], |r| r.get(0)).unwrap()
}

#[test]
fn a_link_waiting_since_before_the_upgrade_is_a_duplicate_after_it() -> Result<()> {
    let dir = TempDir::new()?;
    let active = {
        let repo = Repository::new(dir.path().join("omni.db"))?;
        queue(&repo, SENT, 1).job_id()
    };
    as_before_the_upgrade(&dir, &[active]);

    // The upgrade's start.
    let repo = Repository::new(dir.path().join("omni.db"))?;
    assert_eq!(key_of(&dir, active), omni_core::urlnorm::normalize(SENT));
    match queue(&repo, AGAIN, 2) {
        Enqueued::DuplicateActive { existing_id } => assert_eq!(existing_id, active),
        other => panic!("the waiting job was missed: {other:?}"),
    }
    Ok(())
}

#[test]
fn a_link_delivered_just_before_the_upgrade_is_a_duplicate_after_it() -> Result<()> {
    let dir = TempDir::new()?;
    let done = {
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let done = queue(&repo, SENT, 1).job_id();
        assert!(repo.lease_job_by_id(done, "w", 180)?);
        assert!(repo.finish(done, "w", JobStatus::Completed, None, None, None)?);
        done
    };
    as_before_the_upgrade(&dir, &[done]);

    let repo = Repository::new(dir.path().join("omni.db"))?;
    match queue(&repo, AGAIN, 2) {
        Enqueued::DuplicateRecent { existing_id } => assert_eq!(existing_id, done),
        other => panic!("the recent delivery was missed: {other:?}"),
    }
    Ok(())
}

/// A second chance must not re-queue a job in review whose twin, stored with
/// an old key, is still queued.
#[test]
fn a_second_chance_sees_a_twin_stored_before_the_upgrade() -> Result<()> {
    let dir = TempDir::new()?;
    let (review, twin) = {
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let review = queue(&repo, AGAIN, 1).job_id();
        assert!(repo.lease_job_by_id(review, "w", 180)?);
        assert!(repo.finish(review, "w", JobStatus::RequiresReview, Some("LOGIN_REQUIRED"), Some("login"), None)?);
        let twin = queue(&repo, SENT, 2).job_id();
        (review, twin)
    };
    as_before_the_upgrade(&dir, &[twin]);

    let repo = Repository::new(dir.path().join("omni.db"))?;
    let moved = repo.second_chance(&["LOGIN_REQUIRED"], None, Duration::hours(2), "cookies:facebook.com:20261122", "νέα cookies")?;
    assert!(moved.is_empty(), "the twin is queued: {moved:?}");
    assert_eq!(repo.get_job(review)?.unwrap().status, JobStatus::RequiresReview);
    Ok(())
}

/// With the version current, nothing is read or written again; a database a
/// newer rule set wrote is brought up once, and then left alone.
#[test]
fn the_step_runs_once_per_version() -> Result<()> {
    let dir = TempDir::new()?;
    let id = {
        let repo = Repository::new(dir.path().join("omni.db"))?;
        queue(&repo, SENT, 1).job_id()
    };
    // A key out of date, but the version says current: left as it is.
    {
        let conn = rusqlite::Connection::open(dir.path().join("omni.db"))?;
        conn.execute("UPDATE queue SET url_normalized = ? WHERE id = ?", params![OLD_KEY, id])?;
    }
    let mut conn = rusqlite::Connection::open(dir.path().join("omni.db"))?;
    assert_eq!(Repository::renormalize_if_needed(&mut conn)?, 0);
    drop(Repository::new(dir.path().join("omni.db"))?);
    assert_eq!(key_of(&dir, id), OLD_KEY, "a current version does nothing");

    // An older version: once, then not again.
    conn.execute("UPDATE app_meta SET value = '1' WHERE key = 'url_normalize_version'", [])?;
    assert_eq!(Repository::renormalize_if_needed(&mut conn)?, 1);
    assert_eq!(key_of(&dir, id), omni_core::urlnorm::normalize(SENT));
    assert_eq!(Repository::renormalize_if_needed(&mut conn)?, 0);
    let stored: String = conn.query_row("SELECT value FROM app_meta WHERE key = 'url_normalize_version'", [], |r| r.get(0))?;
    assert_eq!(stored, omni_core::urlnorm::NORMALIZE_VERSION.to_string());
    Ok(())
}

/// A row finished a month ago, whatever its status, `id`s.
fn finished_long_ago(repo: &Repository, dir: &TempDir, n: u32, status: JobStatus) -> i64 {
    let id = queue(repo, SENT, n).job_id();
    assert!(repo.lease_job_by_id(id, "w", 180).unwrap());
    assert!(repo.finish(id, "w", status, None, None, None).unwrap());
    let old = omni_core::timestamps::format(Utc::now() - Duration::days(30));
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    conn.execute("UPDATE queue SET completed_at = ?1, updated_at = ?1, created_at = ?1 WHERE id = ?2", params![old, id]).unwrap();
    id
}

/// P14.8c (a): the one-time pass re-keys EVERY row, a month-old delivery
/// included: brought back later (re-download), it must not let the link in twice.
#[test]
fn the_pass_rekeys_every_row_whatever_its_age() -> Result<()> {
    let dir = TempDir::new()?;
    let id = {
        let repo = Repository::new(dir.path().join("omni.db"))?;
        finished_long_ago(&repo, &dir, 1, JobStatus::Completed)
    };
    as_before_the_upgrade(&dir, &[id]);
    let repo = Repository::new(dir.path().join("omni.db"))?;
    assert_eq!(key_of(&dir, id), omni_core::urlnorm::normalize(SENT));
    // Revived after the upgrade, then the same link sent again: one job.
    assert!(repo.redownload_job(id)?);
    match queue(&repo, AGAIN, 2) {
        Enqueued::DuplicateActive { existing_id } => assert_eq!(existing_id, id),
        other => panic!("the revived job was missed: {other:?}"),
    }
    Ok(())
}

/// P14.8c (b): every path that brings a row back to the queue writes its key
/// by today's rules, even when the one-time pass did not see it (here the
/// version is current, so the pass does nothing): re-download, retry, second
/// chance, the attachment's source.
#[test]
fn every_path_back_to_the_queue_writes_todays_key() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let stale = |id: i64| {
        let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
        conn.execute("UPDATE queue SET url_normalized = ? WHERE id = ?", params![OLD_KEY, id]).unwrap();
    };
    let current = omni_core::urlnorm::normalize(SENT);

    // Re-download of an old delivery.
    let delivered = finished_long_ago(&repo, &dir, 1, JobStatus::Completed);
    stale(delivered);
    assert!(repo.redownload_job(delivered)?);
    assert_eq!(key_of(&dir, delivered), current, "re-download");
    match queue(&repo, AGAIN, 9) {
        Enqueued::DuplicateActive { existing_id } => assert_eq!(existing_id, delivered),
        other => panic!("the re-downloaded job was missed: {other:?}"),
    }
    repo.cancel_job(delivered)?;

    // «Δοκιμή ξανά» on an old failed job, with its own link.
    let failed = finished_long_ago(&repo, &dir, 2, JobStatus::Failed);
    stale(failed);
    assert!(repo.retry_job(failed, None)?);
    assert_eq!(key_of(&dir, failed), current, "retry");
    repo.cancel_job(failed)?;

    // A second chance for a job in review.
    let review = queue(&repo, SENT, 3).job_id();
    assert!(repo.lease_job_by_id(review, "w", 180)?);
    assert!(repo.finish(review, "w", JobStatus::RequiresReview, Some("LOGIN_REQUIRED"), Some("login"), None)?);
    stale(review);
    assert_eq!(repo.second_chance(&["LOGIN_REQUIRED"], None, Duration::hours(2), "cookies:facebook.com:20261122", "νέα cookies")?, vec![review]);
    assert_eq!(key_of(&dir, review), current, "second chance");
    Ok(())
}

/// P11.15a2: one Google Drive file sent in two forms is one job; a row stored
/// with the old (v2) key is brought up by the pass and still blocks the second.
#[test]
fn one_drive_file_sent_in_two_forms_is_one_job_even_across_the_upgrade() -> Result<()> {
    const VIEW: &str = "https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?usp=sharing";
    const OPEN: &str = "https://drive.google.com/open?id=1AbC_dEf-GhIjKlMn";
    let dir = TempDir::new()?;
    let first = {
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let first = queue(&repo, VIEW, 1).job_id();
        match queue(&repo, OPEN, 2) {
            Enqueued::DuplicateActive { existing_id } => assert_eq!(existing_id, first),
            other => panic!("the same file twice: {other:?}"),
        }
        first
    };
    // As P14.8 stored it (v2: the path and `usp` kept), before this version.
    {
        let conn = rusqlite::Connection::open(dir.path().join("omni.db"))?;
        conn.execute("UPDATE queue SET url_normalized = ? WHERE id = ?", params![VIEW, first])?;
        conn.execute("UPDATE app_meta SET value = '2' WHERE key = 'url_normalize_version'", [])?;
    }
    let repo = Repository::new(dir.path().join("omni.db"))?;
    match queue(&repo, OPEN, 3) {
        Enqueued::DuplicateActive { existing_id } => assert_eq!(existing_id, first),
        other => panic!("the row stored with the old key was missed: {other:?}"),
    }
    Ok(())
}
