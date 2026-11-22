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
    assert_eq!(Repository::renormalize_if_needed(&mut conn, Utc::now())?, 0);
    drop(Repository::new(dir.path().join("omni.db"))?);
    assert_eq!(key_of(&dir, id), OLD_KEY, "a current version does nothing");

    // An older version: once, then not again.
    conn.execute("UPDATE app_meta SET value = '1' WHERE key = 'url_normalize_version'", [])?;
    assert_eq!(Repository::renormalize_if_needed(&mut conn, Utc::now())?, 1);
    assert_eq!(key_of(&dir, id), omni_core::urlnorm::normalize(SENT));
    assert_eq!(Repository::renormalize_if_needed(&mut conn, Utc::now())?, 0);
    let stored: String = conn.query_row("SELECT value FROM app_meta WHERE key = 'url_normalize_version'", [], |r| r.get(0))?;
    assert_eq!(stored, omni_core::urlnorm::NORMALIZE_VERSION.to_string());
    Ok(())
}

/// Finished rows older than the window keep their key: no duplicate check meets them.
#[test]
fn old_finished_rows_are_left_alone() -> Result<()> {
    let dir = TempDir::new()?;
    let id = {
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let id = queue(&repo, SENT, 1).job_id();
        assert!(repo.lease_job_by_id(id, "w", 180)?);
        assert!(repo.finish(id, "w", JobStatus::Completed, None, None, None)?);
        id
    };
    as_before_the_upgrade(&dir, &[id]);
    let old = omni_core::timestamps::format(Utc::now() - Duration::days(30));
    {
        let conn = rusqlite::Connection::open(dir.path().join("omni.db"))?;
        conn.execute("UPDATE queue SET completed_at = ?1, updated_at = ?1, created_at = ?1 WHERE id = ?2", params![old, id])?;
    }
    drop(Repository::new(dir.path().join("omni.db"))?);
    assert_eq!(key_of(&dir, id), OLD_KEY);
    Ok(())
}
