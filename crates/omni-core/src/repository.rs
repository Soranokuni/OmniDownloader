use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use std::path::Path;
use std::sync::Arc;
use tracing::info;

use crate::auth::hash_password;
use crate::models::{AuditLog, Job, JobStatus, Journalist, User, UserRole};

#[derive(Clone)]
pub struct Repository {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl Repository {
    pub fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let path = db_path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed creating directory for DB {:?}", parent))?;
        }

        let manager = SqliteConnectionManager::file(path);
        let pool = Pool::builder()
            .max_size(16)
            .build(manager)
            .with_context(|| format!("Failed creating SQLite connection pool for {:?}", path))?;

        let repo = Self {
            pool: Arc::new(pool),
        };
        repo.run_migrations()?;
        Ok(repo)
    }

    fn run_migrations(&self) -> Result<()> {
        let conn = self.pool.get()?;

        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA busy_timeout = 30000;
            PRAGMA foreign_keys = ON;

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
        )?;

        // Check if any admin exists. If not, seed default admin account
        let admin_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE role = 'admin'",
            [],
            |r| r.get(0),
        )?;

        if admin_count == 0 {
            let default_admin_pass = hash_password("admin123")?;
            conn.execute(
                "INSERT INTO users (email, password_hash, role, full_name, is_active)
                 VALUES ('admin@newsroom.local', ?, 'admin', 'System Administrator', 1)",
                params![default_admin_pass],
            )?;
            info!("Seeded initial admin account: admin@newsroom.local (password: admin123)");
        }

        // Check if known journalists are seeded
        let j_count: i64 = conn.query_row("SELECT COUNT(*) FROM journalists", [], |r| r.get(0))?;
        if j_count == 0 {
            let seed_journalists = vec![
                ("PAPADAKI", "Anna Papadaki", vec!["a.papadaki@example.gr", "papadaki@example.gr"]),
                ("NIKOLAOU", "Nikolaou", vec!["nikolaou@example.gr"]),
                ("GEORGIOU", "Antonis Georgiou", vec!["georgiou@example.gr"]),
                ("DIMITRIOU", "Sofia Dimitriou", vec!["dimitriou@example.gr"]),
                ("MCR", "Master Control Room", vec!["mcr@example.gr", "ingest@example.gr"]),
            ];
            for (surname, full_name, emails) in seed_journalists {
                let emails_json = serde_json::to_string(&emails).unwrap_or_else(|_| "[]".into());
                let _ = conn.execute(
                    "INSERT INTO journalists (surname, full_name, emails) VALUES (?, ?, ?)",
                    params![surname, full_name, emails_json],
                );
            }
            info!("Seeded default newsroom journalist roster.");
        }

        Ok(())
    }

    // ==========================================
    // Job Queue Operations
    // ==========================================

    pub fn add_job(
        &self,
        url: &str,
        slug: &str,
        journalist: &str,
        keyword: &str,
        index_str: &str,
        priority: i32,
        status: JobStatus,
        submitted_by: Option<i64>,
        notes: Option<&str>,
        email_source: Option<&str>,
    ) -> Result<i64> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            r#"
            INSERT INTO queue (
                url, slug, journalist, keyword, index_str, priority, status,
                submitted_by_user_id, notes, email_source, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            "#,
        )?;

        let id = stmt.insert(params![
            url,
            slug,
            journalist.to_uppercase(),
            keyword.to_uppercase(),
            index_str,
            priority,
            status.as_str(),
            submitted_by,
            notes,
            email_source
        ])?;

        self.log_audit("INFO", "QUEUE", &format!("Added Job #{} ({}) Status: {}", id, slug, status.as_str()))?;
        Ok(id)
    }

    pub fn lease_next_pending_job(&self) -> Result<Option<Job>> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let maybe_row: Option<(i64, String, String, String, String, String, i32, Option<String>, Option<i64>, Option<String>, Option<String>)> = {
            let mut stmt = tx.prepare(
                r#"
                SELECT id, url, slug, journalist, keyword, index_str, priority, error_message, submitted_by_user_id, email_source, notes
                FROM queue
                WHERE status = 'PENDING'
                ORDER BY priority DESC, created_at ASC
                LIMIT 1
                "#,
            )?;
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                Some((
                    row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
                    row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
                    row.get(10)?
                ))
            } else {
                None
            }
        };

        if let Some((id, url, slug, journalist, keyword, index_str, priority, error_message, submitted_by, email_source, notes)) = maybe_row {
            tx.execute(
                "UPDATE queue SET status = 'DOWNLOADING', progress = 0.0, speed = '0 Mbps', eta = '--:--', updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                params![id],
            )?;
            tx.commit()?;

            return Ok(Some(Job {
                id,
                url,
                slug,
                journalist,
                keyword,
                index_str,
                status: JobStatus::Downloading,
                progress: 0.0,
                speed: "0 Mbps".into(),
                eta: "--:--".into(),
                priority,
                error_message,
                media_format: "Sony XDCAM HD422 1080i50 (MXF RDD9)".into(),
                file_path: None,
                duration_secs: 0.0,
                submitted_by_user_id: submitted_by,
                email_source,
                notes,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }));
        }

        tx.commit()?;
        Ok(None)
    }

    pub fn update_job_status(&self, id: i64, status: JobStatus, error_message: Option<&str>, file_path: Option<&str>, duration: Option<f64>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            r#"
            UPDATE queue
            SET status = ?,
                error_message = COALESCE(?, error_message),
                file_path = COALESCE(?, file_path),
                duration_secs = COALESCE(?, duration_secs),
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
            params![status.as_str(), error_message, file_path, duration, id],
        )?;
        Ok(())
    }

    pub fn update_job_progress(&self, id: i64, progress: f64, speed: &str, eta: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE queue SET progress = ?, speed = ?, eta = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            params![progress, speed, eta, id],
        )?;
        Ok(())
    }

    pub fn get_job(&self, id: i64) -> Result<Option<Job>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM queue WHERE id = ?")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Self::map_job_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn get_all_jobs(&self) -> Result<Vec<Job>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM queue ORDER BY priority DESC, created_at DESC")?;
        let rows = stmt.query_map([], Self::map_job_row)?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    pub fn get_jobs_by_status(&self, status: JobStatus) -> Result<Vec<Job>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM queue WHERE status = ? ORDER BY priority DESC, created_at DESC")?;
        let rows = stmt.query_map(params![status.as_str()], Self::map_job_row)?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    pub fn get_jobs_by_journalist(&self, journalist: &str) -> Result<Vec<Job>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM queue WHERE journalist = ? ORDER BY created_at DESC")?;
        let rows = stmt.query_map(params![journalist.to_uppercase()], Self::map_job_row)?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    pub fn get_user_jobs(&self, user_id: i64) -> Result<Vec<Job>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM queue WHERE submitted_by_user_id = ? ORDER BY created_at DESC")?;
        let rows = stmt.query_map(params![user_id], Self::map_job_row)?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    pub fn retry_job(&self, id: i64, new_url: Option<&str>) -> Result<()> {
        let conn = self.pool.get()?;
        if let Some(url) = new_url {
            conn.execute(
                r#"
                UPDATE queue
                SET url = ?, status = 'PENDING', progress = 0.0, speed = '0 Mbps', eta = '--:--',
                    error_message = NULL, updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                "#,
                params![url, id],
            )?;
        } else {
            conn.execute(
                r#"
                UPDATE queue
                SET status = 'PENDING', progress = 0.0, speed = '0 Mbps', eta = '--:--',
                    error_message = NULL, updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                "#,
                params![id],
            )?;
        }
        self.log_audit("INFO", "QUEUE", &format!("Retried Job #{}", id))?;
        Ok(())
    }

    pub fn delete_job(&self, id: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM queue WHERE id = ?", params![id])?;
        self.log_audit("WARN", "QUEUE", &format!("Deleted Job #{}", id))?;
        Ok(())
    }

    pub fn purge_completed_jobs(&self, older_than_days: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let threshold = Utc::now() - Duration::days(older_than_days);
        let count = conn.execute(
            "DELETE FROM queue WHERE status = 'COMPLETED' AND updated_at < ?",
            params![threshold.to_rfc3339()],
        )?;
        self.log_audit("INFO", "ADMIN", &format!("Purged {} completed jobs older than {} days", count, older_than_days))?;
        Ok(count)
    }

    pub fn clear_failed_jobs(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        let count = conn.execute(
            "DELETE FROM queue WHERE status IN ('FAILED', 'REQUIRES_REVIEW')",
            [],
        )?;
        self.log_audit("WARN", "ADMIN", &format!("Cleared {} failed/review jobs from queue", count))?;
        Ok(count)
    }

    pub fn vacuum_database(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch("VACUUM;")?;
        self.log_audit("INFO", "ADMIN", "SQLite database vacuum executed.")?;
        Ok(())
    }

    fn map_job_row(row: &rusqlite::Row) -> rusqlite::Result<Job> {
        let status_str: String = row.get("status")?;
        Ok(Job {
            id: row.get("id")?,
            url: row.get("url")?,
            slug: row.get("slug")?,
            journalist: row.get("journalist")?,
            keyword: row.get("keyword")?,
            index_str: row.get("index_str")?,
            status: JobStatus::from_str_lossy(&status_str),
            progress: row.get("progress")?,
            speed: row.get("speed")?,
            eta: row.get("eta")?,
            priority: row.get("priority")?,
            error_message: row.get("error_message")?,
            media_format: row.get("media_format")?,
            file_path: row.get("file_path")?,
            duration_secs: row.get("duration_secs")?,
            submitted_by_user_id: row.get("submitted_by_user_id")?,
            email_source: row.get("email_source")?,
            notes: row.get("notes")?,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
    }

    // ==========================================
    // User & Authentication Operations
    // ==========================================

    pub fn create_user(
        &self,
        email: &str,
        password: &str,
        role: UserRole,
        full_name: &str,
        journalist_surname: Option<&str>,
    ) -> Result<i64> {
        let conn = self.pool.get()?;
        let password_hash = hash_password(password)?;
        let mut stmt = conn.prepare(
            "INSERT INTO users (email, password_hash, role, full_name, journalist_surname, is_active)
             VALUES (?, ?, ?, ?, ?, 1)",
        )?;
        let id = stmt.insert(params![
            email.trim().to_lowercase(),
            password_hash,
            role.as_str(),
            full_name,
            journalist_surname
        ])?;
        self.log_audit("INFO", "AUTH", &format!("Created User #{}: {} ({})", id, email, role.as_str()))?;
        Ok(id)
    }

    pub fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM users WHERE email = ? AND is_active = 1")?;
        let mut rows = stmt.query(params![email.trim().to_lowercase()])?;
        if let Some(row) = rows.next()? {
            let role_str: String = row.get("role")?;
            Ok(Some(User {
                id: row.get("id")?,
                email: row.get("email")?,
                password_hash: row.get("password_hash")?,
                role: UserRole::from_str_lossy(&role_str),
                full_name: row.get("full_name")?,
                journalist_surname: row.get("journalist_surname")?,
                is_active: row.get::<_, i32>("is_active")? == 1,
                created_at: Utc::now(),
            }))
        } else {
            Ok(None)
        }
    }

    pub fn get_user_by_id(&self, id: i64) -> Result<Option<User>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM users WHERE id = ?")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            let role_str: String = row.get("role")?;
            Ok(Some(User {
                id: row.get("id")?,
                email: row.get("email")?,
                password_hash: row.get("password_hash")?,
                role: UserRole::from_str_lossy(&role_str),
                full_name: row.get("full_name")?,
                journalist_surname: row.get("journalist_surname")?,
                is_active: row.get::<_, i32>("is_active")? == 1,
                created_at: Utc::now(),
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_users(&self) -> Result<Vec<User>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM users ORDER BY id ASC")?;
        let rows = stmt.query_map([], |row| {
            let role_str: String = row.get("role")?;
            Ok(User {
                id: row.get("id")?,
                email: row.get("email")?,
                password_hash: row.get("password_hash")?,
                role: UserRole::from_str_lossy(&role_str),
                full_name: row.get("full_name")?,
                journalist_surname: row.get("journalist_surname")?,
                is_active: row.get::<_, i32>("is_active")? == 1,
                created_at: Utc::now(),
            })
        })?;
        let mut list = Vec::new();
        for u in rows {
            list.push(u?);
        }
        Ok(list)
    }

    pub fn update_user_password(&self, user_id: i64, new_pass: &str) -> Result<()> {
        let conn = self.pool.get()?;
        let hash = hash_password(new_pass)?;
        conn.execute(
            "UPDATE users SET password_hash = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            params![hash, user_id],
        )?;
        self.log_audit("INFO", "AUTH", &format!("Password updated for User #{}", user_id))?;
        Ok(())
    }

    pub fn set_user_active_status(&self, user_id: i64, active: bool) -> Result<()> {
        let conn = self.pool.get()?;
        let val = if active { 1 } else { 0 };
        conn.execute("UPDATE users SET is_active = ? WHERE id = ?", params![val, user_id])?;
        Ok(())
    }

    pub fn create_session(&self, user_id: i64, token: &str, expires_in_days: i64) -> Result<()> {
        let conn = self.pool.get()?;
        let expires_at = Utc::now() + Duration::days(expires_in_days);
        conn.execute(
            "INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)",
            params![token, user_id, expires_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn get_user_by_session_token(&self, token: &str) -> Result<Option<User>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT u.*
            FROM users u
            JOIN sessions s ON u.id = s.user_id
            WHERE s.token = ? AND s.expires_at > CURRENT_TIMESTAMP AND u.is_active = 1
            "#,
        )?;
        let mut rows = stmt.query(params![token])?;
        if let Some(row) = rows.next()? {
            let role_str: String = row.get("role")?;
            Ok(Some(User {
                id: row.get("id")?,
                email: row.get("email")?,
                password_hash: row.get("password_hash")?,
                role: UserRole::from_str_lossy(&role_str),
                full_name: row.get("full_name")?,
                journalist_surname: row.get("journalist_surname")?,
                is_active: row.get::<_, i32>("is_active")? == 1,
                created_at: Utc::now(),
            }))
        } else {
            Ok(None)
        }
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM sessions WHERE token = ?", params![token])?;
        Ok(())
    }

    // ==========================================
    // Journalist Mapping Operations
    // ==========================================

    pub fn list_journalists(&self) -> Result<Vec<Journalist>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM journalists ORDER BY surname ASC")?;
        let rows = stmt.query_map([], |row| {
            let emails_raw: String = row.get("emails")?;
            let emails: Vec<String> = serde_json::from_str(&emails_raw).unwrap_or_default();
            Ok(Journalist {
                id: row.get("id")?,
                surname: row.get("surname")?,
                full_name: row.get("full_name")?,
                emails,
                default_priority: row.get("default_priority")?,
                created_at: Utc::now(),
            })
        })?;
        let mut list = Vec::new();
        for j in rows {
            list.push(j?);
        }
        Ok(list)
    }

    pub fn save_journalist(&self, surname: &str, full_name: &str, emails: &[String], priority: i32) -> Result<()> {
        let conn = self.pool.get()?;
        let emails_json = serde_json::to_string(emails)?;
        conn.execute(
            r#"
            INSERT INTO journalists (surname, full_name, emails, default_priority)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(surname) DO UPDATE SET
                full_name = excluded.full_name,
                emails = excluded.emails,
                default_priority = excluded.default_priority
            "#,
            params![surname.to_uppercase(), full_name, emails_json, priority],
        )?;
        Ok(())
    }

    pub fn delete_journalist(&self, surname: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM journalists WHERE surname = ?", params![surname.to_uppercase()])?;
        Ok(())
    }

    pub fn find_journalist_by_email(&self, email: &str) -> Result<Option<String>> {
        let clean_email = email.trim().to_lowercase();
        let journalists = self.list_journalists()?;
        for j in journalists {
            for em in j.emails {
                if em.trim().to_lowercase() == clean_email {
                    return Ok(Some(j.surname));
                }
            }
        }
        Ok(None)
    }

    // ==========================================
    // Audit Logging
    // ==========================================

    pub fn log_audit(&self, level: &str, category: &str, message: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO audit_logs (level, category, message, timestamp) VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
            params![level, category, message],
        )?;
        Ok(())
    }

    pub fn get_recent_audit_logs(&self, limit: usize) -> Result<Vec<AuditLog>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM audit_logs ORDER BY id DESC LIMIT ?")?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(AuditLog {
                id: row.get("id")?,
                level: row.get("level")?,
                category: row.get("category")?,
                message: row.get("message")?,
                timestamp: Utc::now(),
            })
        })?;
        let mut list = Vec::new();
        for l in rows {
            list.push(l?);
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_repository_lifecycle() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;

        // 1. User CRUD
        let user_id = repo.create_user("journalist@station.gr", "hash123", UserRole::User, "Maria Papadaki", Some("PAPADAKI"))?;
        let user = repo.get_user_by_id(user_id)?.expect("User must exist");
        assert_eq!(user.email, "journalist@station.gr");
        assert_eq!(user.journalist_surname.as_deref(), Some("PAPADAKI"));

        // 2. Journalist Mapping
        repo.save_journalist("PAPADAKI", "Maria Papadaki", &["m.papadaki@station.gr".to_string()], 10)?;
        let mapped_surname = repo.find_journalist_by_email("m.papadaki@station.gr")?;
        assert_eq!(mapped_surname.as_deref(), Some("PAPADAKI"));

        // 3. Queue Job Insertion & Atomic Leasing
        let job_id = repo.add_job(
            "https://www.youtube.com/watch?v=12345",
            "1_PAPADAKI_PARADE",
            "PAPADAKI",
            "PARADE",
            "1",
            10,
            JobStatus::Pending,
            Some(user_id),
            Some("ΓΙΑ ΠΛΑΝΑ"),
            Some("newsroom@station.gr"),
        )?;
        assert!(job_id > 0);

        let leased = repo.lease_next_pending_job()?.expect("Job must be leased");
        assert_eq!(leased.id, job_id);
        assert_eq!(leased.status, JobStatus::Downloading);

        // 4. Progress and Status Updates
        repo.update_job_progress(job_id, 45.5, "12.4MB/s", "00:15")?;
        repo.update_job_status(job_id, JobStatus::Transcoding, None, None, None)?;

        let current_job = repo.get_job(job_id)?.expect("Job must exist");
        assert_eq!(current_job.status, JobStatus::Transcoding);
        assert_eq!(current_job.progress, 45.5);

        // 5. Completion
        repo.update_job_status(job_id, JobStatus::Completed, None, Some("C:/Watchfolder/1_PAPADAKI_PARADE.mxf"), Some(120.5))?;
        let completed_job = repo.get_job(job_id)?.expect("Job must exist");
        assert_eq!(completed_job.status, JobStatus::Completed);
        assert_eq!(completed_job.duration_secs, 120.5);

        // 6. Audit Logging
        repo.log_audit("INFO", "INGEST", "Job ingested cleanly")?;
        let audits = repo.get_recent_audit_logs(5)?;
        assert!(!audits.is_empty());
        assert_eq!(audits[0].category, "INGEST");

        Ok(())
    }
}


