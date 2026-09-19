//! Ordered schema migrations (plan P0.2).
//!
//! The previous `run_migrations` was a single `CREATE TABLE IF NOT EXISTS`
//! batch. That cannot express "add a column", "drop a constraint" or "rewrite
//! legacy rows", so every schema change from Phase 1 onward would have required
//! operators to delete `omni.db` — losing the archive of what went to air.
//!
//! Rules for adding a migration:
//!
//! * Append to [`MIGRATIONS`]; never edit or renumber an existing entry. A
//!   deployed daemon has already recorded that version as applied.
//! * Each entry runs exactly once, inside a transaction. A failure rolls the
//!   whole entry back and aborts start-up rather than leaving a half-migrated
//!   database feeding playout.
//! * The database is backed up before any pending migration runs.

use anyhow::{Context, Result};
use rusqlite::Connection;
use tracing::{info, warn};

/// Skip the pre-migration backup above this size; copying a multi-GB file at
/// every start-up would delay ingest more than the backup is worth.
const MAX_BACKUP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// `(version, sql)`, applied in ascending order. Append only.
pub const MIGRATIONS: &[(u32, &str)] = &[
    (
        1,
        // Baseline: the schema as it shipped before the hardening plan. Written
        // with IF NOT EXISTS so an existing production database adopts version 1
        // without any table being recreated.
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            email TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            role TEXT NOT NULL CHECK(role IN ('admin', 'open_mcr', 'user')),
            full_name TEXT NOT NULL,
            journalist_surname TEXT DEFAULT NULL,
            is_active INTEGER DEFAULT 1,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS sessions (
            token TEXT PRIMARY KEY,
            user_id INTEGER NOT NULL,
            expires_at DATETIME NOT NULL,
            FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS queue (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            url TEXT NOT NULL UNIQUE,
            slug TEXT NOT NULL,
            journalist TEXT NOT NULL DEFAULT 'MCR',
            keyword TEXT NOT NULL DEFAULT 'ASSET',
            index_str TEXT NOT NULL DEFAULT '1',
            status TEXT NOT NULL,
            progress REAL DEFAULT 0.0,
            speed TEXT DEFAULT '0 Mbps',
            eta TEXT DEFAULT '--:--',
            priority INTEGER DEFAULT 0,
            error_message TEXT DEFAULT NULL,
            media_format TEXT DEFAULT 'Sony XDCAM HD422 1080i50 (MXF RDD9)',
            file_path TEXT DEFAULT NULL,
            duration_secs REAL DEFAULT 0.0,
            submitted_by_user_id INTEGER DEFAULT NULL,
            email_source TEXT DEFAULT NULL,
            notes TEXT DEFAULT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(submitted_by_user_id) REFERENCES users(id)
        );

        CREATE TABLE IF NOT EXISTS journalists (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            surname TEXT NOT NULL UNIQUE,
            full_name TEXT NOT NULL,
            emails TEXT NOT NULL,
            default_priority INTEGER DEFAULT 0,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS audit_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            level TEXT NOT NULL,
            category TEXT NOT NULL,
            message TEXT NOT NULL,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        "#,
    ),
    (
        2,
        // Plan P1.1: explicit job state machine with leases, per-stage timing,
        // error codes and idempotent enqueue.
        //
        // The UNIQUE constraint on queue.url has to go (defect D-10): the same
        // link legitimately recurs -- a different journalist, a re-send after a
        // discard, an MCR override onto an already-known URL. SQLite cannot drop
        // a constraint, so the table is rebuilt. Dedup moves to url_normalized
        // plus a time window, which is a policy the code can reason about rather
        // than a hard error from the database.
        r#"
        CREATE TABLE queue_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            url TEXT NOT NULL,
            url_normalized TEXT,
            slug TEXT NOT NULL,
            journalist TEXT NOT NULL DEFAULT 'MCR',
            keyword TEXT NOT NULL DEFAULT 'ASSET',
            index_str TEXT NOT NULL DEFAULT '1',
            status TEXT NOT NULL,
            stage TEXT NOT NULL DEFAULT 'QUEUED',
            progress REAL DEFAULT 0.0,
            speed TEXT DEFAULT '0 Mbps',
            eta TEXT DEFAULT '--:--',
            priority INTEGER DEFAULT 0,
            attempts INTEGER NOT NULL DEFAULT 0,
            max_attempts INTEGER NOT NULL DEFAULT 3,
            lease_owner TEXT,
            lease_expires_at TEXT,
            stage_started_at TEXT,
            not_before TEXT,
            error_message TEXT DEFAULT NULL,
            error_code TEXT DEFAULT NULL,
            media_format TEXT DEFAULT 'Sony XDCAM HD422 1080i50 (MXF RDD9)',
            file_path TEXT DEFAULT NULL,
            source_path TEXT DEFAULT NULL,
            compliance_json TEXT DEFAULT NULL,
            candidates_json TEXT DEFAULT NULL,
            extraction_method TEXT DEFAULT NULL,
            stage_timings_json TEXT DEFAULT NULL,
            duration_secs REAL DEFAULT 0.0,
            submitted_by_user_id INTEGER DEFAULT NULL,
            email_source TEXT DEFAULT NULL,
            email_message_id TEXT DEFAULT NULL,
            notes TEXT DEFAULT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            delivered_at TEXT DEFAULT NULL,
            completed_at TEXT DEFAULT NULL,
            FOREIGN KEY(submitted_by_user_id) REFERENCES users(id)
        );

        INSERT INTO queue_new (
            id, url, slug, journalist, keyword, index_str, status, progress, speed, eta,
            priority, error_message, media_format, file_path, duration_secs,
            submitted_by_user_id, email_source, notes, created_at, updated_at
        )
        SELECT
            id, url, slug, journalist, keyword, index_str, status, progress, speed, eta,
            priority, error_message, media_format, file_path, duration_secs,
            submitted_by_user_id, email_source, notes, created_at, updated_at
        FROM queue;

        DROP TABLE queue;
        ALTER TABLE queue_new RENAME TO queue;

        -- Rows left mid-flight by the old code have no owner and no lease, so
        -- nothing will ever finish them. They are orphans by definition: record
        -- the stage they died in, then requeue.
        UPDATE queue SET stage = 'EXTRACT'   WHERE status = 'EXTRACTING';
        UPDATE queue SET stage = 'DOWNLOAD'  WHERE status = 'DOWNLOADING';
        UPDATE queue SET stage = 'TRANSCODE' WHERE status = 'TRANSCODING';
        UPDATE queue SET stage = 'REWRAP'    WHERE status = 'REWRAPPING';
        UPDATE queue SET status = 'PENDING', stage = 'QUEUED'
            WHERE status IN ('EXTRACTING','DOWNLOADING','TRANSCODING','REWRAPPING');
        UPDATE queue SET stage = 'DONE' WHERE status IN ('COMPLETED','COMPLETED_MANUAL');

        CREATE INDEX idx_queue_status_prio ON queue(status, priority DESC, created_at ASC);
        CREATE INDEX idx_queue_url_norm    ON queue(url_normalized);
        CREATE INDEX idx_queue_lease       ON queue(status, lease_expires_at);

        -- Per-job timeline shown in the MCR job drawer.
        CREATE TABLE job_events (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id  INTEGER NOT NULL,
            at      TEXT NOT NULL,
            stage   TEXT,
            level   TEXT,
            message TEXT,
            FOREIGN KEY(job_id) REFERENCES queue(id) ON DELETE CASCADE
        );
        CREATE INDEX idx_job_events_job ON job_events(job_id, id);

        -- Journalist aliases for Greek name resolution (plan P4.3), as a JSON
        -- array. Declared here so the roster survives a Phase 4 restart.
        ALTER TABLE journalists ADD COLUMN aliases TEXT NOT NULL DEFAULT '[]';
        "#,
    ),
];

/// Connection pragmas applied to every pooled connection.
///
/// `synchronous=NORMAL` is the right trade under WAL: it keeps the durability
/// that matters (no corruption on crash) while removing an fsync per commit,
/// and the queue writes a progress row several times a second per job.
pub const CONNECTION_PRAGMAS: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA busy_timeout = 30000;
    PRAGMA foreign_keys = ON;
    PRAGMA synchronous = NORMAL;
";

/// Highest version defined in this build.
pub fn target_version() -> u32 {
    MIGRATIONS.last().map(|(v, _)| *v).unwrap_or(0)
}

/// Version currently recorded in the database (0 for a fresh file).
pub fn current_version(conn: &Connection) -> Result<u32> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
         );",
    )?;
    let v: Option<u32> = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap_or(None);
    Ok(v.unwrap_or(0))
}

/// Apply every migration newer than the recorded version.
///
/// `db_path` is used for the pre-migration backup; pass `None` for in-memory
/// databases in tests.
pub fn apply(conn: &mut Connection, db_path: Option<&std::path::Path>) -> Result<u32> {
    let from = current_version(conn)?;
    let to = target_version();
    if from >= to {
        return Ok(from);
    }

    if let Some(path) = db_path {
        backup_before_migration(path, from);
    }

    for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > from) {
        info!("Applying schema migration {} -> {}", version - 1, version);
        let tx = conn.transaction().with_context(|| {
            format!("Failed opening a transaction for schema migration {version}")
        })?;
        tx.execute_batch(sql)
            .with_context(|| format!("Schema migration {version} failed; database unchanged"))?;
        tx.execute(
            "INSERT INTO schema_version (version, applied_at)
             VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            rusqlite::params![version],
        )?;
        tx.commit()
            .with_context(|| format!("Failed committing schema migration {version}"))?;
    }

    info!("Database schema is at version {to}");
    Ok(to)
}

/// Copy the database next to itself before migrating.
///
/// Best effort: a failed backup logs a warning but does not stop the daemon —
/// refusing to start would take the newsroom off automated ingest over a
/// housekeeping problem. A failed *migration* does stop it.
fn backup_before_migration(db_path: &std::path::Path, from_version: u32) {
    if !db_path.exists() {
        return;
    }
    match std::fs::metadata(db_path) {
        Ok(meta) if meta.len() > MAX_BACKUP_BYTES => {
            warn!(
                "Skipping pre-migration backup: {:?} is {} MB, above the {} MB limit",
                db_path,
                meta.len() / 1_048_576,
                MAX_BACKUP_BYTES / 1_048_576
            );
            return;
        }
        Err(e) => {
            warn!("Could not stat {:?} for pre-migration backup: {e}", db_path);
            return;
        }
        _ => {}
    }

    let dir = match db_path.parent() {
        Some(p) => p.join("backups"),
        None => return,
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("Could not create backup directory {:?}: {e}", dir);
        return;
    }

    let name = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "omni.db".into());
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let dest = dir.join(format!("{name}.v{from_version}.{stamp}"));

    match std::fs::copy(db_path, &dest) {
        Ok(_) => info!("Pre-migration backup written to {:?}", dest),
        Err(e) => warn!("Pre-migration backup to {:?} failed: {e}", dest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_migrates_to_the_target_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(current_version(&conn).unwrap(), 0);
        let v = apply(&mut conn, None).unwrap();
        assert_eq!(v, target_version());
        // The baseline tables must exist.
        for table in ["users", "sessions", "queue", "journalists", "audit_logs"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table {table} missing after migration");
        }
    }

    #[test]
    fn migrations_are_idempotent_and_preserve_data() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, None).unwrap();
        conn.execute(
            "INSERT INTO queue (url, slug, status) VALUES ('https://x/1', '1_MCR_TEST', 'PENDING')",
            [],
        )
        .unwrap();

        // Running again must be a no-op, not a re-creation.
        let v = apply(&mut conn, None).unwrap();
        assert_eq!(v, target_version());
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "existing job rows were lost by a re-run");

        // Exactly one row per applied version.
        let applied: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied as usize, MIGRATIONS.len());
    }

    #[test]
    fn versions_are_unique_and_ascending() {
        // Guards against a merge that duplicates or reorders an entry, which
        // would silently skip a migration on an already-deployed database.
        let mut last = 0;
        for (v, _) in MIGRATIONS {
            assert!(*v > last, "migration versions must ascend: {v} after {last}");
            last = *v;
        }
    }

    #[test]
    fn backup_is_written_before_migrating_an_existing_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("omni.db");

        // Opening creates the file, so `apply` has an existing database to back
        // up before it runs anything.
        let mut conn = Connection::open(&db).unwrap();
        apply(&mut conn, Some(&db)).unwrap();

        let backups: Vec<_> = std::fs::read_dir(tmp.path().join("backups"))
            .expect("backups directory should exist")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(backups.len(), 1, "expected exactly one backup file");
        assert!(backups[0]
            .file_name()
            .to_string_lossy()
            .starts_with("omni.db.v0."));

        // A second start-up has nothing to migrate, so it must not spam the
        // backup directory on every service restart.
        let mut conn = Connection::open(&db).unwrap();
        apply(&mut conn, Some(&db)).unwrap();
        let after = std::fs::read_dir(tmp.path().join("backups")).unwrap().count();
        assert_eq!(after, 1, "a no-op start-up wrote another backup");
    }

    /// Upgrading a real v1 database: the rows survive, the UNIQUE(url)
    /// constraint is gone, and jobs the old code left mid-flight are requeued.
    #[test]
    fn v1_database_upgrades_without_losing_jobs() {
        let mut conn = Connection::open_in_memory().unwrap();

        // Build a v1 database exactly as the old code would have left it.
        conn.execute_batch(MIGRATIONS[0].1).unwrap();
        conn.execute_batch(
            "INSERT INTO queue (url, slug, status) VALUES
                ('https://www.youtube.com/watch?v=a', '1_PAPADAKI_ONE', 'COMPLETED'),
                ('https://www.youtube.com/watch?v=b', '2_PAPADAKI_TWO', 'DOWNLOADING'),
                ('https://www.youtube.com/watch?v=c', '3_PAPADAKI_TRE', 'TRANSCODING'),
                ('https://www.youtube.com/watch?v=d', '4_PAPADAKI_FOR', 'REQUIRES_REVIEW');",
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (1, '2026-01-01T00:00:00.000Z')",
            [],
        )
        .unwrap();

        apply(&mut conn, None).unwrap();

        // Nothing lost.
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 4, "jobs were lost rebuilding the queue table");

        // Jobs the old code abandoned mid-flight are requeued, not stranded
        // forever in DOWNLOADING/TRANSCODING with no owner (defect D-02).
        let requeued: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM queue WHERE status='PENDING' AND stage='QUEUED'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(requeued, 2, "orphaned in-flight jobs were not requeued");

        // Terminal states are untouched.
        let completed: i64 = conn
            .query_row("SELECT COUNT(*) FROM queue WHERE status='COMPLETED'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(completed, 1);
        let review: i64 = conn
            .query_row("SELECT COUNT(*) FROM queue WHERE status='REQUIRES_REVIEW'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(review, 1);

        // UNIQUE(url) is gone (defect D-10): the same link must be queueable
        // again for a different journalist or after a discard.
        conn.execute(
            "INSERT INTO queue (url, slug, status) VALUES ('https://www.youtube.com/watch?v=a', '9_NIKOLAOU_AGAIN', 'PENDING')",
            [],
        )
        .expect("re-queuing a previously seen URL must be allowed");

        // New columns are present and defaulted.
        let (attempts, max_attempts, stage): (i64, i64, String) = conn
            .query_row(
                "SELECT attempts, max_attempts, stage FROM queue WHERE slug='9_NIKOLAOU_AGAIN'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((attempts, max_attempts, stage.as_str()), (0, 3, "QUEUED"));
    }
}
