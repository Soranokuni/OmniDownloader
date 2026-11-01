//! Scheduled retention (plan P1.21): finished jobs 90 days, audit 365, handled-mail
//! records 90, expired sessions; and the indexes the polled queries need.
//! Invented names and URLs only.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use tempfile::TempDir;

use omni_core::migrations;
use omni_core::models::{Enqueued, JobStatus, NewJob};
use omni_core::repository::{
    Repository, AUDIT_KEEP_DAYS, INBOX_JOB_SQL, INBOX_MAIL_SQL, JOB_KEEP_DAYS, MAIL_KEEP_DAYS, PURGE_AUDIT_SQL,
    PURGE_JOB_IDS_SQL, PURGE_MAIL_SQL, RETENTION_BATCH, USER_JOBS_SQL,
};
use omni_core::timestamps;

struct Fixture {
    _dir: TempDir,
    repo: Repository,
    conn: Connection,
    n: i64,
}

fn open_conn(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 30000;").unwrap();
    conn
}

fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("omni.db");
    let repo = Repository::new(&path).unwrap();
    let conn = open_conn(&path);
    Fixture { _dir: dir, repo, conn, n: 0 }
}

/// The legacy schema-default form, `YYYY-MM-DD HH:MM:SS`.
fn legacy(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

impl Fixture {
    /// A job made by `enqueue`, then set to `status` with the given timestamps.
    fn job(&mut self, status: &str, completed_at: Option<String>, updated_at: String) -> i64 {
        self.n += 1;
        let job = NewJob::new(format!("https://video{}.example/clip", self.n), format!("SLUG_{}", self.n), "Testopoulos");
        let id = match self.repo.enqueue(&job, 0).unwrap() {
            Enqueued::Created { id } => id,
            other => panic!("not created: {other:?}"),
        };
        self.conn
            .execute(
                "UPDATE queue SET status = ?, completed_at = ?, updated_at = ?, created_at = ? WHERE id = ?",
                params![status, completed_at, updated_at, updated_at, id],
            )
            .unwrap();
        id
    }

    fn count(&self, table: &str) -> i64 {
        self.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    fn exists(&self, id: i64) -> bool {
        self.conn.query_row("SELECT COUNT(*) FROM queue WHERE id = ?", params![id], |r| r.get::<_, i64>(0)).unwrap() > 0
    }

    fn children(&self, id: i64) {
        self.conn
            .execute("INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, 'x', 'QUEUED', 'INFO', 'm')", params![id])
            .unwrap();
        self.conn.execute("INSERT INTO repair_attempts (job_id, started_at, mode) VALUES (?, 'x', 'hold')", params![id]).unwrap();
        self.conn
            .execute("INSERT INTO job_evidence (job_id, path, bytes, created_at) VALUES (?, 'p.json', 1, 'x')", params![id])
            .unwrap();
    }

    fn rows_for(&self, table: &str, id: i64) -> i64 {
        self.conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE job_id = ?"), params![id], |r| r.get(0)).unwrap()
    }
}

#[test]
fn a_completed_job_goes_after_90_days_by_either_timestamp_form() -> Result<()> {
    let mut f = fixture();
    let now = Utc::now();
    let d = |days: i64| now - Duration::days(days);
    let new_done = f.job("COMPLETED", Some(timestamps::format(d(89))), timestamps::format(d(89)));
    let old_done = f.job("COMPLETED", Some(timestamps::format(d(91))), timestamps::format(d(91)));
    // completed_at NULL: updated_at decides, in the legacy form.
    let new_legacy = f.job("COMPLETED_MANUAL", None, legacy(d(89)));
    let old_legacy = f.job("COMPLETED_MANUAL", None, legacy(d(91)));
    // completed_at wins over a recent updated_at.
    let old_done_touched = f.job("COMPLETED", Some(timestamps::format(d(91))), timestamps::format(d(1)));

    let c = f.repo.purge_old_records(now)?;
    assert_eq!(c.jobs, 3);
    assert!(f.exists(new_done) && f.exists(new_legacy), "89-day-old jobs are kept");
    assert!(!f.exists(old_done) && !f.exists(old_legacy) && !f.exists(old_done_touched), "91-day-old jobs go");
    assert_eq!(JOB_KEEP_DAYS, 90);
    Ok(())
}

#[test]
fn every_terminal_status_is_purged_and_no_other() -> Result<()> {
    let mut f = fixture();
    let now = Utc::now();
    let old = timestamps::format(now - Duration::days(400));
    let mut gone = vec![];
    let mut kept = vec![];
    for s in JobStatus::ALL {
        let id = f.job(s.as_str(), Some(old.clone()), old.clone());
        if s.is_terminal() { gone.push((s, id)) } else { kept.push((s, id)) }
    }
    assert!(kept.iter().any(|(s, _)| *s == JobStatus::Pending));
    assert!(kept.iter().any(|(s, _)| *s == JobStatus::Running));
    assert!(kept.iter().any(|(s, _)| *s == JobStatus::AwaitingApproval));
    f.repo.purge_old_records(now)?;
    for (s, id) in gone {
        assert!(!f.exists(id), "{s:?} is terminal and 400 days old");
    }
    for (s, id) in kept {
        assert!(f.exists(id), "{s:?} must never be purged");
    }
    Ok(())
}

#[test]
fn a_held_output_keeps_its_job() -> Result<()> {
    let mut f = fixture();
    let now = Utc::now();
    let old = timestamps::format(now - Duration::days(400));
    let held = f.job("COMPLETED", Some(old.clone()), old.clone());
    let plain = f.job("COMPLETED", Some(old.clone()), old.clone());
    f.conn.execute(
        "INSERT INTO held_outputs (job_id, path, bytes, sha256, created_at, expires_at) VALUES (?, 'h.mxf', 1, 'ab', 'x', 'y')",
        params![held],
    )?;
    f.children(held);
    f.repo.purge_old_records(now)?;
    assert!(f.exists(held), "a held output's job stays");
    assert!(f.rows_for("job_events", held) >= 2, "and so does its timeline");
    assert!(!f.exists(plain));
    Ok(())
}

#[test]
fn a_purged_jobs_children_go_with_it_and_a_kept_jobs_stay() -> Result<()> {
    let mut f = fixture();
    let now = Utc::now();
    let old = timestamps::format(now - Duration::days(120));
    let fresh = timestamps::format(now - Duration::days(2));
    let doomed = f.job("FAILED", Some(old.clone()), old.clone());
    let kept = f.job("FAILED", Some(fresh.clone()), fresh);
    f.children(doomed);
    f.children(kept);
    let c = f.repo.purge_old_records(now)?;
    for t in ["job_events", "repair_attempts", "job_evidence"] {
        assert_eq!(f.rows_for(t, doomed), 0, "{t} rows of the purged job");
        assert!(f.rows_for(t, kept) >= 1, "{t} rows of the kept job");
    }
    assert_eq!((c.jobs, c.evidence), (1, 1));
    assert!(c.job_events >= 2, "enqueue wrote one timeline row, the test another");
    Ok(())
}

#[test]
fn orphaned_timeline_rows_are_swept() -> Result<()> {
    let f = fixture();
    f.conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    f.conn.execute("INSERT INTO job_events (job_id, at, stage, level, message) VALUES (987654, 'x', 'QUEUED', 'INFO', 'm')", [])?;
    let c = f.repo.purge_old_records(Utc::now())?;
    assert_eq!(c.job_events, 1);
    assert_eq!(f.count("job_events"), 0);
    Ok(())
}

#[test]
fn audit_entries_go_after_365_days() -> Result<()> {
    let f = fixture();
    let now = Utc::now();
    for (days, msg) in [(500, "legacy-old"), (366, "old"), (364, "recent")] {
        let ts = if msg == "legacy-old" { legacy(now - Duration::days(days)) } else { timestamps::format(now - Duration::days(days)) };
        f.conn.execute("INSERT INTO audit_logs (level, category, message, timestamp) VALUES ('INFO', 'T', ?, ?)", params![msg, ts])?;
    }
    let c = f.repo.purge_old_records(now)?;
    assert_eq!(c.audit, 2);
    let left: Vec<String> = f
        .conn
        .prepare("SELECT message FROM audit_logs WHERE category = 'T'")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(left, ["recent"]);
    assert_eq!(AUDIT_KEEP_DAYS, 365);
    Ok(())
}

#[test]
fn handled_mail_records_go_after_90_days() -> Result<()> {
    let f = fixture();
    let now = Utc::now();
    let ins = |id: &str, received: Option<String>, processed: String| {
        f.conn
            .execute(
                "INSERT INTO processed_mail (internet_message_id, processed_at, received_at, outcome) VALUES (?, ?, ?, 'ok')",
                params![id, processed, received],
            )
            .unwrap();
    };
    let d = |days: i64| timestamps::format(now - Duration::days(days));
    ins("recv-89", Some(d(89)), d(1));
    ins("recv-91", Some(d(91)), d(1)); // received_at decides, not the fresh processed_at
    ins("null-89", None, d(89));
    ins("null-91", None, d(91));
    ins("legacy-91", None, legacy(now - Duration::days(91)));
    let c = f.repo.purge_old_records(now)?;
    assert_eq!(c.mail, 3);
    let mut left: Vec<String> = f
        .conn
        .prepare("SELECT internet_message_id FROM processed_mail")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    left.sort();
    assert_eq!(left, ["null-89", "recv-89"]);
    assert_eq!(MAIL_KEEP_DAYS, 90);
    Ok(())
}

#[test]
fn expired_sessions_are_removed_and_live_ones_stay() -> Result<()> {
    let f = fixture();
    let uid = f.repo.create_user("tester@example.invalid", "correct horse battery", omni_core::models::UserRole::Admin, "Test User", None)?;
    f.repo.create_session(uid, "live-token", 7)?;
    f.repo.create_session(uid, "dead-token", 7)?;
    f.conn.execute("UPDATE sessions SET expires_at = ? WHERE token = 'dead-token'", params![timestamps::format(Utc::now() - Duration::days(1))])?;
    let c = f.repo.purge_old_records(Utc::now())?;
    assert_eq!(c.sessions, 1);
    assert_eq!(f.count("sessions"), 1);
    Ok(())
}

#[test]
fn an_audit_table_of_only_old_rows_is_never_left_empty_so_no_default_admin_returns() -> Result<()> {
    let f = fixture();
    // The state that matters: no users and an audit trail of old entries only.
    f.conn.execute("DELETE FROM audit_logs", [])?;
    assert_eq!(f.count("users"), 0);
    for i in 0..3 {
        f.conn.execute(
            "INSERT INTO audit_logs (level, category, message, timestamp) VALUES ('INFO', 'T', ?, ?)",
            params![format!("old {i}"), timestamps::format(Utc::now() - Duration::days(400))],
        )?;
    }
    let c = f.repo.purge_old_records(Utc::now())?;
    assert_eq!(c.audit, 2, "the newest old entry is never deleted");
    assert_eq!(f.count("audit_logs"), 2, "the newest old entry and the retention row");
    let retention: i64 = f.conn.query_row("SELECT COUNT(*) FROM audit_logs WHERE category = 'retention'", [], |r| r.get(0))?;
    assert_eq!(retention, 1);
    assert!(!f.repo.ensure_default_admin()?, "an emptied audit table would have let Admin / Admin back in");
    assert_eq!(f.count("users"), 0);
    Ok(())
}

#[test]
fn a_run_that_removes_nothing_writes_no_audit_row() -> Result<()> {
    let f = fixture();
    let before = f.count("audit_logs");
    let c = f.repo.purge_old_records(Utc::now())?;
    assert_eq!(c.total(), 0);
    assert_eq!(f.count("audit_logs"), before);
    Ok(())
}

#[test]
fn more_than_one_batch_is_removed_in_one_call() -> Result<()> {
    let f = fixture();
    let now = Utc::now();
    let old = timestamps::format(now - Duration::days(500));
    let total = RETENTION_BATCH * 2 + 123;
    let tx = f.conn.unchecked_transaction()?;
    for i in 0..total {
        tx.execute(
            "INSERT INTO audit_logs (level, category, message, timestamp) VALUES ('INFO', 'T', 'x', ?)",
            params![old],
        )?;
        tx.execute(
            "INSERT INTO processed_mail (internet_message_id, processed_at, outcome) VALUES (?, ?, 'ok')",
            params![format!("m{i}"), old],
        )?;
        tx.execute(
            "INSERT INTO queue (url, slug, status, created_at, updated_at, completed_at) VALUES (?, 's', 'COMPLETED', ?, ?, ?)",
            params![format!("https://bulk{i}.example/v"), old, old, old],
        )?;
    }
    tx.commit()?;
    let c = f.repo.purge_old_records(now)?;
    assert_eq!((c.audit as i64, c.mail as i64, c.jobs as i64), (total - 1, total, total));
    assert_eq!(f.count("queue"), 0);
    assert_eq!(f.count("processed_mail"), 0);
    assert_eq!(f.count("audit_logs"), 2, "the newest old entry and the retention row");
    Ok(())
}

/// A database one migration behind this build, with rows, as a daemon upgraded from the previous build has.
fn previous_database(path: &std::path::Path) -> Connection {
    let mut conn = Connection::open(path).unwrap();
    migrations::current_version(&conn).unwrap();
    for (v, sql) in migrations::MIGRATIONS.iter().filter(|(v, _)| *v < migrations::target_version()) {
        let tx = conn.transaction().unwrap();
        tx.execute_batch(sql).unwrap();
        tx.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", params![v]).unwrap();
        tx.commit().unwrap();
    }
    conn
}

#[test]
fn migration_31_upgrades_the_previous_schema_database_without_touching_its_rows() -> Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("omni.db");
    {
        let conn = previous_database(&path);
        assert_eq!(migrations::current_version(&conn)?, migrations::target_version() - 1);
        conn.execute("INSERT INTO queue (url, slug, status) VALUES ('https://a.example/v', 's', 'PENDING')", [])?;
        conn.execute("INSERT INTO audit_logs (level, category, message) VALUES ('INFO', 'T', 'm')", [])?;
        conn.execute("INSERT INTO processed_mail (internet_message_id, processed_at, outcome) VALUES ('id1', 'x', 'ok')", [])?;
    }
    let _repo = Repository::new(&path)?;
    let conn = open_conn(&path);
    let n = |t: &str| conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!((n("queue"), n("processed_mail")), (1, 1));
    assert!(n("audit_logs") >= 1);
    let max: u32 = conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?;
    assert_eq!(max, migrations::target_version());
    let idx: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN
         ('idx_queue_user_created','idx_queue_created','idx_queue_counts','idx_processed_mail_when','idx_audit_logs_ts','idx_sessions_expires')",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(idx, 6);
    Ok(())
}

/// Plan lines that read a whole table: a `SCAN <table>` with no index in it.
fn bare_scans(conn: &Connection, sql: &str, nparams: usize, tables: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let args: Vec<&dyn rusqlite::ToSql> = (0..nparams).map(|_| &"2000-01-01T00:00:00.000Z" as &dyn rusqlite::ToSql).collect();
    let lines: Vec<String> = stmt
        .query_map(args.as_slice(), |r| r.get::<_, String>(3))
        .unwrap()
        .map(|l| l.unwrap())
        .collect();
    let bad = lines
        .iter()
        .filter(|l| l.starts_with("SCAN") && !l.contains("USING") && tables.iter().any(|t| l.contains(t)))
        .cloned()
        .collect();
    (bad, lines)
}

/// (name, sql, bound parameters, tables that must not be scanned bare, an index the plan must name)
fn polled() -> Vec<(&'static str, &'static str, usize, &'static [&'static str], &'static str)> {
    vec![
        ("get_user_jobs", USER_JOBS_SQL, 1, &["queue"], "idx_queue_user_created"),
        ("inbox_job_rows", INBOX_JOB_SQL, 2, &["queue"], "idx_queue_created"),
        ("inbox_mail_rows", INBOX_MAIL_SQL, 1, &["processed_mail"], "idx_processed_mail_when"),
        ("purge jobs", PURGE_JOB_IDS_SQL, 2, &["queue"], "idx_queue_"),
        ("purge audit", PURGE_AUDIT_SQL, 2, &["audit_logs"], "idx_audit_logs_ts"),
        ("purge mail", PURGE_MAIL_SQL, 2, &["processed_mail"], "idx_processed_mail_when"),
    ]
}

fn plan_problems(conn: &Connection) -> Vec<String> {
    let mut problems = vec![];
    for (name, sql, n, tables, index) in polled() {
        let (bad, lines) = bare_scans(conn, sql, n, tables);
        if !bad.is_empty() {
            problems.push(format!("{name}: bare scan {bad:?}"));
        }
        if !lines.iter().any(|l| l.contains(index)) {
            problems.push(format!("{name}: plan does not use {index}: {lines:?}"));
        }
    }
    problems
}

#[test]
fn the_polled_and_purge_queries_use_an_index() {
    let f = fixture();
    let problems = plan_problems(&f.conn);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn negative_control_the_plan_check_fails_without_migration_31_indexes() {
    let f = fixture();
    for idx in [
        "idx_queue_user_created",
        "idx_queue_created",
        "idx_queue_counts",
        "idx_processed_mail_when",
        "idx_audit_logs_ts",
        "idx_sessions_expires",
    ] {
        f.conn.execute(&format!("DROP INDEX {idx}"), []).unwrap();
    }
    let problems = plan_problems(&f.conn);
    assert!(problems.len() >= 4, "the check must notice the missing indexes: {problems:#?}");
}

#[test]
fn the_audit_purge_alone_never_removes_the_newest_entry() -> Result<()> {
    // The state after an audit purge whose retention row could not be written
    // (a later part failed): with no users, an empty table would bring Admin / Admin back.
    let f = fixture();
    f.conn.execute("DELETE FROM audit_logs", [])?;
    for i in 0..5 {
        f.conn.execute(
            "INSERT INTO audit_logs (level, category, message, timestamp) VALUES ('INFO', 'T', ?, ?)",
            params![format!("old {i}"), timestamps::format(Utc::now() - Duration::days(900 + i))],
        )?;
    }
    let cutoff = timestamps::format(Utc::now() - Duration::days(AUDIT_KEEP_DAYS));
    let removed = f.conn.execute(PURGE_AUDIT_SQL, params![cutoff, RETENTION_BATCH])?;
    assert_eq!(removed, 4);
    assert_eq!(f.count("audit_logs"), 1, "at least one row survives, whatever else happens");
    let msg: String = f.conn.query_row("SELECT message FROM audit_logs", [], |r| r.get(0))?;
    assert_eq!(msg, "old 4", "the one with the highest id");
    assert!(!f.repo.ensure_default_admin()?);
    Ok(())
}

