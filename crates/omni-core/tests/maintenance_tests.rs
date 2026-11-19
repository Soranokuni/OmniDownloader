//! The database looks after itself (P11.13): the nightly online backup, its
//! rotation, and the integrity check, on real SQLite files. Every name is
//! invented.

use std::path::Path;

use chrono::NaiveDate;
use rusqlite::Connection;

use omni_core::maintenance::{backup_name, integrity_check, nightly_backup, INTEGRITY_PROBLEM};
use omni_core::models::NewJob;
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn queue(repo: &Repository, n: u32) -> i64 {
    repo.enqueue(&NewJob::new(format!("https://news.example/story-{n}"), format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id()
}

fn rows_in(path: &Path) -> i64 {
    Connection::open(path).unwrap().query_row("SELECT COUNT(*) FROM queue", [], |r| r.get(0)).unwrap()
}

/// The backup is a whole, checked copy of the live database taken while it is
/// written to, it replaces the same day's file, and nothing but our own
/// `omni-YYYY-MM-DD.db` files is ever removed: 7 daily and 4 weekly stay.
#[test]
fn the_nightly_backup_is_a_checked_copy_and_keeps_seven_daily_and_four_weekly() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("omni.db");
    let repo = Repository::new(&db).unwrap();
    for n in 0..50 {
        queue(&repo, n);
    }
    let backups = dir.path().join("backups");
    std::fs::create_dir_all(backups.join("taxonomy")).unwrap();
    std::fs::write(backups.join("taxonomy").join("taxonomy-manual.json"), b"{}").unwrap();
    std::fs::write(backups.join("notes.txt"), b"kept").unwrap();
    std::fs::write(backups.join("omni-2026-11-01.db.tmp"), b"cut short").unwrap();
    std::fs::write(backups.join("omni-2026-11-01.db.tmp-wal"), b"cut short").unwrap();
    // Sixty earlier nights' backups.
    let today = d("2026-11-19");
    for n in 1..=60 {
        std::fs::write(backups.join(backup_name(today - chrono::Duration::days(n))), b"old").unwrap();
    }

    // Workers keep writing while the copy is made.
    let writer = {
        let repo = repo.clone();
        std::thread::spawn(move || {
            for n in 100..300 {
                queue(&repo, n);
            }
        })
    };
    let report = nightly_backup(&repo, std::fs::metadata(&db).unwrap().len(), &backups, today).unwrap();
    writer.join().unwrap();

    assert_eq!(report.path, backups.join("omni-2026-11-19.db"));
    assert!(report.bytes > 0);
    assert_eq!(Repository::quick_check_file(&report.path).unwrap(), vec!["ok".to_string()]);
    let copied = rows_in(&report.path);
    assert!((50..=250).contains(&copied), "a consistent snapshot of the live queue: {copied}");

    let mut ours: Vec<String> = std::fs::read_dir(&backups)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("omni-"))
        .collect();
    ours.sort();
    assert_eq!(ours.len(), 11, "{ours:?}");
    assert_eq!(report.removed, 61 - 11);
    assert!(!backups.join("omni-2026-11-01.db.tmp").exists(), "a cut copy is cleared");
    assert!(!backups.join("omni-2026-11-01.db.tmp-wal").exists(), "and its journal");
    let journal: String = Connection::open(&report.path).unwrap().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(journal, "delete", "one self-contained file, no -wal beside it");
    assert!(backups.join("notes.txt").exists() && backups.join("taxonomy").join("taxonomy-manual.json").exists(), "nothing else is touched");
    assert!(db.exists(), "the live database stays where it is");

    // Run again the same day (a "Run now"): the day's file is replaced, nothing else changes.
    queue(&repo, 999);
    let again = nightly_backup(&repo, 0, &backups, today).unwrap();
    assert_eq!(again.path, report.path);
    assert_eq!(rows_in(&again.path), rows_in(&db));
    assert_eq!(again.removed, 0);
}

/// The health lines come from the stored task rows, so they survive a restart:
/// a failed backup turns `maintenance` amber naming it, the next good run clears
/// it, and `database` appears once the integrity check has run.
#[test]
fn a_failed_task_shows_on_the_health_lines_from_the_stored_rows() {
    use omni_core::health::{checks, Health, HealthState};
    use omni_core::scheduler::{default_tasks, TaskOutcome};
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    repo.ensure_scheduled_tasks(&default_tasks()).unwrap();
    let health = HealthState::new();
    omni_core::maintenance::refresh_health(&repo, &health);
    assert_eq!(health.get(checks::MAINTENANCE).unwrap().state, Health::Ok);
    assert!(health.get(checks::DATABASE).is_none(), "no check has run yet");

    repo.record_task_run("db_backup", TaskOutcome::Failed, Some("not enough space"), 10).unwrap();
    omni_core::maintenance::refresh_health(&repo, &health);
    let line = health.get(checks::MAINTENANCE).unwrap();
    assert_eq!(line.state, Health::Degraded);
    assert!(line.detail.unwrap().contains("db_backup"));

    repo.record_task_run("db_backup", TaskOutcome::Ok, None, 10).unwrap();
    repo.record_task_run("db_check", TaskOutcome::Ok, None, 10).unwrap();
    let after_restart = HealthState::new();
    omni_core::maintenance::refresh_health(&repo, &after_restart);
    assert_eq!(after_restart.get(checks::MAINTENANCE).unwrap().state, Health::Ok);
    let db = after_restart.get(checks::DATABASE).unwrap();
    assert_eq!(db.state, Health::Ok);
    assert!(db.detail.unwrap().contains("last backup"));
}

/// A sound database passes; a damaged one fails with the integrity prefix the
/// `database` health line turns red on.
#[test]
fn the_integrity_check_tells_a_damaged_database_apart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("omni.db");
    {
        let repo = Repository::new(&db).unwrap();
        queue(&repo, 1);
        integrity_check(&repo).expect("a fresh database is sound");
    }
    // A table and index of our own at the end of the file, then its last page scribbled over.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA journal_mode = DELETE; CREATE TABLE scratch_t (x TEXT); CREATE INDEX scratch_i ON scratch_t (x);").unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        for n in 0..4000 {
            tx.execute("INSERT INTO scratch_t (x) VALUES (?1)", rusqlite::params![format!("row-{n:06}-padding-padding")]).unwrap();
        }
        tx.commit().unwrap();
    }
    let mut bytes = std::fs::read(&db).unwrap();
    let len = bytes.len();
    for b in &mut bytes[len - 3000..len - 100] {
        *b = 0xA5;
    }
    std::fs::write(&db, bytes).unwrap();

    let found = Repository::quick_check_file(&db).unwrap();
    assert_ne!(found, vec!["ok".to_string()], "the scribbled page is noticed");
    let repo = Repository::new(&db).unwrap();
    let err = integrity_check(&repo).expect_err("a damaged database fails");
    assert!(format!("{err:#}").starts_with(INTEGRITY_PROBLEM), "{err:#}");
}
