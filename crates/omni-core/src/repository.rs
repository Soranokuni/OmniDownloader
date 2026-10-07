use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use rusqlite::OptionalExtension;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

use crate::auth::hash_password;
use crate::migrations;
use crate::models::{
    AuditLog, Enqueued, Job, JobEvent, JobStage, JobStatus, Journalist, LoginAttempt, NewJob,
    ProcessedMail, QueueSummary, User,
    UserRole,
};
use crate::taxonomy::{Group, ImportReport, Person, Taxonomy};
use crate::timestamps;

/// Default window for treating a re-sent link as already delivered.
///
/// A day covers the realistic case (the same story forwarded again during one
/// news cycle) without blocking a genuine re-ingest the next day.
pub const DEFAULT_DEDUP_WINDOW_HOURS: i64 = 24;

#[derive(Clone)]
pub struct Repository {
    pool: Arc<Pool<SqliteConnectionManager>>,
    /// Used for the pre-migration backup; `None` for in-memory test databases.
    db_path: Option<Arc<Path>>,
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
            db_path: Some(Arc::from(path.to_path_buf())),
        };
        repo.run_migrations()?;
        Ok(repo)
    }

    fn run_migrations(&self) -> Result<()> {
        let mut conn = self.pool.get()?;

        conn.execute_batch(migrations::CONNECTION_PRAGMAS)?;
        migrations::apply(&mut conn, self.db_path.as_deref())
            .context("Schema migration failed; refusing to start against a half-migrated database")?;

        // No default admin is seeded (plan P2.4, defect W-04).
        //
        // The old code created `admin@newsroom.local / admin123` on every
        // database that had no admin — including production. Published
        // credentials on a box that can repoint the playout watchfolder is not
        // a convenience, it is a back door, and nothing ever removed it.
        //
        // First run is instead: `omni-ingest setup`, or `/setup` served to a
        // loopback client while no admin exists. Once an admin exists, that
        // window closes.
        let admin_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE role = 'admin' AND is_active = 1",
            [],
            |r| r.get(0),
        )?;
        if admin_count == 0 {
            warn!(
                "No administrator account exists. Run `omni-ingest setup`, or open /setup in a \
                 browser ON THIS MACHINE, to create one. Until then the panels cannot be \
                 administered."
            );
        }

        self.seed_journalists_from_file(&conn)?;
        self.seed_taxonomy_from_file(&conn)?;

        Ok(())
    }

    /// Seed the journalist roster from `data/journalists.seed.json`, if present
    /// and the table is empty.
    ///
    /// The roster is real newsroom staff -- names and work addresses -- so it
    /// lives in a deployment data file, not in source control. Ship
    /// `data/journalists.seed.example.json` as the shape and let each station
    /// provide its own. A missing file is normal: the admin panel is the
    /// primary way to manage the roster, and `MCR` is always present as the
    /// fallback for unresolved journalists.
    fn seed_journalists_from_file(&self, conn: &rusqlite::Connection) -> Result<()> {
        let existing: i64 = conn.query_row("SELECT COUNT(*) FROM journalists", [], |r| r.get(0))?;
        if existing > 0 {
            return Ok(());
        }

        // MCR is structural, not staff: the parser assigns it whenever it
        // cannot resolve a journalist, and delivery uses it as a folder name.
        conn.execute(
            "INSERT INTO journalists (surname, full_name, emails) VALUES ('MCR', 'Master Control Room', '[]')",
            [],
        )?;

        let seed_file = match self.db_path.as_deref().and_then(|p| p.parent()) {
            Some(dir) => dir.join("journalists.seed.json"),
            None => return Ok(()),
        };
        let raw = match std::fs::read_to_string(&seed_file) {
            Ok(raw) => raw,
            Err(_) => {
                info!(
                    "No journalist roster at {:?}; add journalists in Admin -> Journalists",
                    seed_file
                );
                return Ok(());
            }
        };

        #[derive(serde::Deserialize)]
        struct SeedJournalist {
            surname: String,
            #[serde(default)]
            full_name: String,
            #[serde(default)]
            emails: Vec<String>,
            #[serde(default)]
            default_priority: i32,
            #[serde(default)]
            aliases: Vec<String>,
        }

        let roster: Vec<SeedJournalist> = serde_json::from_str(&raw)
            .with_context(|| format!("Failed parsing journalist roster {:?}", seed_file))?;

        let mut seeded = 0usize;
        for j in roster {
            let surname = j.surname.trim().to_uppercase();
            if surname.is_empty() || surname == "MCR" {
                continue;
            }
            let emails_json = serde_json::to_string(&j.emails).unwrap_or_else(|_| "[]".into());
            let full_name = if j.full_name.is_empty() {
                surname.clone()
            } else {
                j.full_name
            };
            let aliases_json = serde_json::to_string(&j.aliases).unwrap_or_else(|_| "[]".into());
            conn.execute(
                "INSERT OR IGNORE INTO journalists (surname, full_name, emails, default_priority, aliases)
                 VALUES (?, ?, ?, ?, ?)",
                params![surname, full_name, emails_json, j.default_priority, aliases_json],
            )?;
            seeded += 1;
        }
        info!("Seeded {seeded} journalists from {:?}", seed_file);
        Ok(())
    }

    // ==========================================
    // Job Queue Operations
    // ==========================================

    /// Convenience wrapper around [`Repository::enqueue`].
    ///
    /// Kept for the existing call sites (web form, email watcher) while Phase 4
    /// moves them to `NewJob`. It routes through `enqueue`, so rows created this
    /// way still get a `url_normalized` dedup key and the journalist's priority
    /// floor -- a direct INSERT here would create jobs invisible to dedup.
    ///
    /// Returns the job id, which for a duplicate is the *existing* job's id.
    /// Callers that need to tell the difference should use `enqueue` directly.
    #[allow(clippy::too_many_arguments)]
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
        let job = NewJob {
            url: url.to_string(),
            slug: slug.to_string(),
            journalist: journalist.to_string(),
            keyword: keyword.to_string(),
            index_str: index_str.to_string(),
            priority,
            status,
            submitted_by_user_id: submitted_by,
            notes: notes.map(str::to_string),
            email_source: email_source.map(str::to_string),
            email_message_id: None,
            extraction_method: None,
            group_code: None,
            max_videos: None,
            parent_job_id: None,
        };
        Ok(self.enqueue(&job, DEFAULT_DEDUP_WINDOW_HOURS)?.job_id())
    }


    /// Queue a job, deduplicating by normalized URL (plan P1.1, defect D-10).
    ///
    /// Three outcomes rather than an error, because "we already have this" is a
    /// normal thing for a newsroom to do -- two journalists forward the same
    /// story, or an email is re-sent after a correction:
    ///
    /// * an active job with the same normalized URL -> `DuplicateActive`;
    ///   queuing it again would put two identical files in the watchfolder.
    /// * the same URL completed for the same journalist inside
    ///   `dedup_window_hours` -> `DuplicateRecent`; the reply tells them it is
    ///   already delivered.
    /// * otherwise a new job.
    ///
    /// A *different* journalist re-queuing a completed URL is deliberately not a
    /// duplicate: they need their own slug in their own folder.
    ///
    /// Priority is raised to the journalist's default if that is higher
    /// (defect D-20 -- `journalists.default_priority` was never applied).
    pub fn enqueue(&self, job: &NewJob, dedup_window_hours: i64) -> Result<Enqueued> {
        let normalized = crate::urlnorm::normalize(&job.url);
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Already queued or running?
        let active: Option<i64> = tx
            .query_row(
                "SELECT id FROM queue
                 WHERE url_normalized = ? AND status IN ('PENDING','RUNNING')
                 ORDER BY id ASC LIMIT 1",
                params![normalized],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing_id) = active {
            tx.commit()?;
            self.log_audit(
                "INFO",
                "QUEUE",
                &format!("Duplicate of active job #{existing_id} ignored: {}", job.url),
            )?;
            return Ok(Enqueued::DuplicateActive { existing_id });
        }

        // Recently delivered for this same journalist?
        let cutoff = timestamps::format(Utc::now() - Duration::hours(dedup_window_hours));
        let recent: Option<i64> = tx
            .query_row(
                "SELECT id FROM queue
                 WHERE url_normalized = ?
                   AND journalist = ?
                   AND status IN ('COMPLETED','COMPLETED_MANUAL')
                   AND COALESCE(completed_at, updated_at) >= ?
                 ORDER BY id DESC LIMIT 1",
                params![normalized, job.journalist.to_uppercase(), cutoff],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing_id) = recent {
            tx.commit()?;
            self.log_audit(
                "INFO",
                "QUEUE",
                &format!("Duplicate of recently completed job #{existing_id}: {}", job.url),
            )?;
            return Ok(Enqueued::DuplicateRecent { existing_id });
        }

        let journalist = job.journalist.to_uppercase();
        let default_priority: i32 = tx
            .query_row(
                "SELECT default_priority FROM journalists WHERE surname = ?",
                params![journalist],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let priority = job.priority.max(default_priority);

        let now = timestamps::now_string();
        tx.execute(
            r#"
            INSERT INTO queue (
                url, url_normalized, slug, journalist, keyword, index_str, priority,
                status, stage, submitted_by_user_id, notes, email_source,
                email_message_id, extraction_method, group_code, max_videos, parent_job_id,
                created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'QUEUED', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                job.url,
                normalized,
                job.slug,
                journalist,
                job.keyword.to_uppercase(),
                job.index_str,
                priority,
                job.status.as_str(),
                job.submitted_by_user_id,
                job.notes,
                job.email_source,
                job.email_message_id,
                job.extraction_method,
                job.group_code,
                job.max_videos.filter(|n| *n > 0),
                job.parent_job_id,
                now,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, 'QUEUED', 'INFO', ?)",
            params![id, now, format!("Queued as {} (priority {priority})", job.slug)],
        )?;
        tx.commit()?;

        self.log_audit(
            "INFO",
            "QUEUE",
            &format!("Added Job #{id} ({}) status {}", job.slug, job.status.as_str()),
        )?;
        Ok(Enqueued::Created { id })
    }

    /// Finish a leased job in a terminal state and release the lease.
    ///
    /// Ownership is checked: a worker whose lease was reaped mid-stage must not
    /// be able to mark the job completed after somebody else picked it up.
    /// Returns `false` when the job was no longer ours.
    pub fn finish(
        &self,
        job_id: i64,
        owner: &str,
        status: JobStatus,
        error_code: Option<&str>,
        error_message: Option<&str>,
        file_path: Option<&str>,
    ) -> Result<bool> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let delivered = matches!(status, JobStatus::Completed);
        let changed = conn.execute(
            r#"
            UPDATE queue
            SET status = ?,
                stage = CASE WHEN ? = 'COMPLETED' THEN 'DONE' ELSE stage END,
                lease_owner = NULL,
                lease_expires_at = NULL,
                error_code = ?,
                error_message = ?,
                file_path = COALESCE(?, file_path),
                progress = CASE WHEN ? = 'COMPLETED' THEN 100.0 ELSE progress END,
                completed_at = ?,
                delivered_at = CASE WHEN ? THEN ? ELSE delivered_at END,
                updated_at = ?
            WHERE id = ? AND lease_owner = ?
            "#,
            params![
                status.as_str(),
                status.as_str(),
                error_code,
                error_message,
                file_path,
                status.as_str(),
                now,
                delivered,
                now,
                now,
                job_id,
                owner
            ],
        )?;
        if changed == 1 {
            let level = if status == JobStatus::Completed { "INFO" } else { "ERROR" };
            self.record_event(
                job_id,
                level,
                None,
                &match error_code {
                    Some(code) => format!("Finished as {} ({code})", status.as_str()),
                    None => format!("Finished as {}", status.as_str()),
                },
            )?;
        }
        Ok(changed == 1)
    }

    /// Release a lease and requeue the job after a backoff (plan P1.9).
    ///
    /// Used for the retryable error codes: transient network trouble, a full
    /// disk, a delivery share that was briefly unreachable. `not_before` keeps
    /// the worker from immediately re-leasing the same job and burning all its
    /// attempts inside a second.
    pub fn requeue_after(
        &self,
        job_id: i64,
        owner: &str,
        backoff: Duration,
        error_code: &str,
        error_message: &str,
    ) -> Result<bool> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let not_before = timestamps::format(Utc::now() + backoff);
        let changed = conn.execute(
            "UPDATE queue SET status='PENDING', stage='QUEUED', lease_owner=NULL,
             lease_expires_at=NULL, not_before=?, error_code=?, error_message=?,
             progress=0.0, updated_at=?
             WHERE id=? AND lease_owner=?",
            params![not_before, error_code, error_message, now, job_id, owner],
        )?;
        if changed == 1 {
            self.record_event(
                job_id,
                "WARN",
                Some(JobStage::Queued),
                &format!(
                    "{error_code}: retrying in {} s -- {error_message}",
                    backoff.num_seconds()
                ),
            )?;
        }
        Ok(changed == 1)
    }

    /// Store the pre-delivery compliance report (plan P1.6).
    ///
    /// Kept whether the job passed or failed: on a failure it is what the MCR
    /// drawer shows the operator, and on a success it is the record of what was
    /// verified about a file that is now on air.
    pub fn set_compliance_report(&self, job_id: i64, report_json: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE queue SET compliance_json = ?, updated_at = ? WHERE id = ?",
            params![report_json, timestamps::now_string(), job_id],
        )?;
        Ok(())
    }

    /// Jobs currently leased by this host, for the status endpoint and the
    /// start-up temp sweep.
    pub fn running_job_ids(&self) -> Result<Vec<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT id FROM queue WHERE status = 'RUNNING'")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
    }

    /// Lease the highest-priority ready job for `owner`.
    ///
    /// The caller **must already hold a worker permit** before calling this
    /// (plan P1.1, defect D-01). The old code leased first and then waited on
    /// the semaphore, so with two workers and twenty pending jobs, eighteen sat
    /// in DOWNLOADING with nobody working on them and the MCR panel showed
    /// eighteen phantom downloads.
    ///
    /// Runs in one IMMEDIATE transaction so two workers cannot select the same
    /// row before either updates it.
    pub fn lease_job(&self, owner: &str, lease_secs: i64) -> Result<Option<Job>> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let now = timestamps::now_string();
        let id: Option<i64> = tx
            .query_row(
                r#"
                SELECT id FROM queue
                WHERE status = 'PENDING'
                  AND (not_before IS NULL OR not_before <= ?)
                ORDER BY priority DESC, created_at ASC
                LIMIT 1
                "#,
                params![now],
                |r| r.get(0),
            )
            .optional()?;

        let id = match id {
            Some(id) => id,
            None => {
                tx.commit()?;
                return Ok(None);
            }
        };

        let expires = timestamps::format(Utc::now() + Duration::seconds(lease_secs));
        tx.execute(
            r#"
            UPDATE queue
            SET status = 'RUNNING',
                stage = 'EXTRACT',
                lease_owner = ?,
                lease_expires_at = ?,
                stage_started_at = ?,
                attempts = attempts + 1,
                not_before = NULL,
                progress = 0.0,
                speed = '0 Mbps',
                eta = '--:--',
                updated_at = ?
            WHERE id = ?
            "#,
            params![owner, expires, now, now, id],
        )?;
        tx.execute(
            "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, 'EXTRACT', 'INFO', ?)",
            params![id, now, format!("Leased by {owner}")],
        )?;
        tx.commit()?;

        self.get_job(id)
    }

    /// Extend the lease. Called every ~30 s by the worker that holds it.
    ///
    /// Returns `false` when the job is no longer ours -- the reaper requeued it,
    /// or an operator cancelled it. The worker must then abandon its work
    /// rather than deliver a file for a job somebody else now owns.
    pub fn heartbeat(&self, job_id: i64, owner: &str, lease_secs: i64) -> Result<bool> {
        let conn = self.pool.get()?;
        let expires = timestamps::format(Utc::now() + Duration::seconds(lease_secs));
        let changed = conn.execute(
            "UPDATE queue SET lease_expires_at = ?, updated_at = ?
             WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![expires, timestamps::now_string(), job_id, owner],
        )?;
        Ok(changed == 1)
    }

    /// Move a leased job to the next stage and reset the stage clock.
    pub fn set_stage(&self, job_id: i64, owner: &str, stage: JobStage) -> Result<bool> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let changed = conn.execute(
            "UPDATE queue SET stage = ?, stage_started_at = ?, updated_at = ?
             WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![stage.as_str(), now, now, job_id, owner],
        )?;
        if changed == 1 {
            conn.execute(
                "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, ?, 'INFO', ?)",
                params![job_id, now, stage.as_str(), format!("Stage {}", stage.as_str())],
            )?;
        }
        Ok(changed == 1)
    }

    /// Give a running job a new index and slug — `1` becomes `1A` when the
    /// sniffer finds sibling videos in its article. Only the worker holding
    /// the lease may do it, like every other change to a running job.
    pub fn rename_leased_job(&self, job_id: i64, owner: &str, index_str: &str, slug: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let changed = conn.execute(
            "UPDATE queue SET index_str = ?, slug = ?, updated_at = ?
             WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![index_str, slug, now, job_id, owner],
        )?;
        if changed == 1 {
            conn.execute(
                "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, 'EXTRACT', 'INFO', ?)",
                params![job_id, now, format!("Renamed to {slug}: the article has more videos")],
            )?;
        }
        Ok(changed == 1)
    }

    /// MCR gives a job a better keyword (P7.12): `{index}_{JOURNALIST}_{KEYWORD}`
    /// becomes its slug, the file name it is delivered under. Only while the
    /// file is not being made yet: waiting, needing review, or running up to
    /// the transcode (the worker reads the slug again when it starts the
    /// rewrap, which names the clip). One statement, so a job that moves on
    /// meanwhile is refused rather than half-renamed. `(old, new)` slug, or
    /// `None` when it is too late (or the job is gone).
    pub fn rename_job_keyword(&self, job_id: i64, keyword: &str) -> Result<Option<(String, String)>> {
        let Some(job) = self.get_job(job_id)? else {
            return Ok(None);
        };
        let slug = format!("{}_{}_{}", job.index_str, job.journalist, keyword);
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE queue SET keyword = ?, slug = ?, updated_at = ?
             WHERE id = ?
               AND (status IN ('PENDING', 'REQUIRES_REVIEW', 'MANUAL_DOWNLOAD', 'FAILED')
                    OR (status = 'RUNNING' AND stage IN ('QUEUED', 'EXTRACT', 'DOWNLOAD', 'TRANSCODE')))",
            params![keyword, slug, timestamps::now_string(), job_id],
        )?;
        Ok((changed == 1).then_some((job.slug, slug)))
    }

    /// Correct a running job's address (a word the sender glued to it,
    /// plan P3.12), dedup key included, so the desk opens and dedups the
    /// address that works. Only the worker holding the lease may do it.
    pub fn set_leased_job_url(&self, job_id: i64, owner: &str, url: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE queue SET url = ?, url_normalized = ?, updated_at = ?
             WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![url, crate::urlnorm::normalize(url), timestamps::now_string(), job_id, owner],
        )?;
        Ok(changed == 1)
    }

    /// Store the videos offered to MCR for this job (`candidates_json`).
    pub fn set_candidates(&self, job_id: i64, candidates_json: Option<&str>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE queue SET candidates_json = ?, updated_at = ? WHERE id = ?",
            params![candidates_json, timestamps::now_string(), job_id],
        )?;
        Ok(())
    }

    /// Append to the job's timeline. Shown in the MCR job drawer.
    pub fn record_event(
        &self,
        job_id: i64,
        level: &str,
        stage: Option<JobStage>,
        message: &str,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, ?, ?, ?)",
            params![
                job_id,
                timestamps::now_string(),
                stage.map(|s| s.as_str()),
                level,
                message
            ],
        )?;
        Ok(())
    }

    /// The job's timeline, oldest first.
    pub fn get_job_events(&self, job_id: i64, limit: usize) -> Result<Vec<JobEvent>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, job_id, at, stage, level, message FROM job_events
             WHERE job_id = ? ORDER BY id ASC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![job_id, limit as i64], |row| {
            Ok(JobEvent {
                id: row.get(0)?,
                job_id: row.get(1)?,
                at: timestamps::parse_opt(row.get(2).ok()),
                stage: row.get(3)?,
                level: row.get(4)?,
                message: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// The newest `limit` lines of the job's timeline, oldest first: a job
    /// retried many times keeps its latest story, not its first.
    pub fn recent_job_events(&self, job_id: i64, limit: usize) -> Result<Vec<JobEvent>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, job_id, at, stage, level, message FROM (
                 SELECT * FROM job_events WHERE job_id = ? ORDER BY id DESC LIMIT ?
             ) ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![job_id, limit as i64], |row| {
            Ok(JobEvent {
                id: row.get(0)?,
                job_id: row.get(1)?,
                at: timestamps::parse_opt(row.get(2).ok()),
                stage: row.get(3)?,
                level: row.get(4)?,
                message: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Requeue jobs whose lease expired (plan P1.1).
    ///
    /// A lease expires when the worker died without releasing it -- a crash, a
    /// power cut, a `taskkill`. Jobs with attempts left go back to PENDING;
    /// the rest go to REQUIRES_REVIEW so a human sees them rather than the
    /// daemon retrying the same failure forever.
    ///
    /// Returns `(requeued, sent_to_review)`.
    pub fn reap_expired_leases(&self) -> Result<(usize, usize)> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = timestamps::now_string();

        let expired: Vec<(i64, i32, i32)> = {
            let mut stmt = tx.prepare(
                "SELECT id, attempts, max_attempts FROM queue
                 WHERE status = 'RUNNING' AND lease_expires_at IS NOT NULL AND lease_expires_at < ?",
            )?;
            let rows = stmt.query_map(params![now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let (mut requeued, mut review) = (0usize, 0usize);
        for (id, attempts, max_attempts) in expired {
            if attempts < max_attempts {
                tx.execute(
                    "UPDATE queue SET status='PENDING', stage='QUEUED', lease_owner=NULL,
                     lease_expires_at=NULL, progress=0.0, updated_at=? WHERE id=?",
                    params![now, id],
                )?;
                tx.execute(
                    "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, 'QUEUED', 'WARN', ?)",
                    params![
                        id,
                        now,
                        format!("Lease expired; requeued (attempt {attempts}/{max_attempts})")
                    ],
                )?;
                requeued += 1;
            } else {
                tx.execute(
                    "UPDATE queue SET status='REQUIRES_REVIEW', lease_owner=NULL, lease_expires_at=NULL,
                     error_code='LEASE_EXPIRED', error_message=?, updated_at=? WHERE id=?",
                    params![
                        format!("Lease expired after {attempts} attempts without completing"),
                        now,
                        id
                    ],
                )?;
                tx.execute(
                    "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, NULL, 'ERROR', ?)",
                    params![id, now, "Lease expired; attempts exhausted, sent to review"],
                )?;
                review += 1;
            }
        }
        tx.commit()?;
        Ok((requeued, review))
    }

    /// Requeue jobs this host was running when it died (plan P1.1, defect D-02).
    ///
    /// Called once at start-up, before the worker pool starts. Without it, a
    /// crash during a transcode strands the job in RUNNING forever: no worker
    /// owns it, no lease will expire that anyone is watching, and the operator
    /// sees a job that has been "transcoding" since Tuesday.
    ///
    /// Only this host's jobs are touched, so two daemons sharing a database
    /// never steal each other's work.
    pub fn recover_on_startup(&self, hostname: &str) -> Result<usize> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let prefix = format!("{hostname}:%");

        let ids: Vec<i64> = {
            let mut stmt =
                conn.prepare("SELECT id FROM queue WHERE status = 'RUNNING' AND lease_owner LIKE ?")?;
            let rows = stmt.query_map(params![prefix], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for id in &ids {
            conn.execute(
                "UPDATE queue SET status='PENDING', stage='QUEUED', lease_owner=NULL,
                 lease_expires_at=NULL, progress=0.0, speed='0 Mbps', eta='--:--', updated_at=?
                 WHERE id=?",
                params![now, id],
            )?;
            conn.execute(
                "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, 'QUEUED', 'WARN', ?)",
                params![id, now, "Recovered after daemon restart; requeued"],
            )?;
        }
        if !ids.is_empty() {
            self.log_audit(
                "WARN",
                "QUEUE",
                &format!("Recovered {} orphaned job(s) after restart", ids.len()),
            )?;
        }
        Ok(ids.len())
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
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
            WHERE id = ?
            "#,
            params![status.as_str(), error_message, file_path, duration, id],
        )?;
        Ok(())
    }

    pub fn update_job_progress(&self, id: i64, progress: f64, speed: &str, eta: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE queue SET progress = ?, speed = ?, eta = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
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

    /// Queue depth by status, plus the age of the oldest waiting job.
    ///
    /// One query rather than `get_all_jobs().len()`: the status panel refreshes
    /// every few seconds and the archive grows without bound, so counting rows
    /// by loading them would make the panel slower the longer the station runs
    /// (defect W-12's smaller sibling).
    pub fn queue_summary(&self) -> Result<QueueSummary> {
        let conn = self.pool.get()?;

        let mut summary = QueueSummary::default();
        let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM queue GROUP BY status")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (status, count) = row?;
            match status.as_str() {
                "PENDING" => summary.pending += count,
                "REQUIRES_REVIEW" => summary.review += count,
                "MANUAL_DOWNLOAD" => summary.manual += count,
                "COMPLETED" | "COMPLETED_MANUAL" => summary.completed += count,
                "FAILED" => summary.failed += count,
                // Everything else is a stage of "currently being worked on".
                _ => summary.running += count,
            }
            summary.total += count;
        }

        // How long the front of the queue has been waiting. This is the number
        // that tells an operator the pipeline has stalled, which a plain
        // pending count does not: twenty pending jobs are normal after a big
        // rundown and alarming an hour later.
        let oldest: Option<String> = conn
            .query_row(
                "SELECT MIN(created_at) FROM queue WHERE status = 'PENDING'",
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        summary.oldest_pending_age_secs = oldest
            .and_then(|s| timestamps::parse_opt(Some(s)))
            .map(|t| (Utc::now() - t).num_seconds().max(0));

        Ok(summary)
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

    /// One page of an MCR desk list (plan P7.1). The WHERE clauses are
    /// fixed text chosen by `view`; every value is a bound parameter.
    pub fn list_jobs_page(
        &self,
        view: crate::models::JobsView,
        filter: &crate::models::JobsFilter,
        page: i64,
        per_page: i64,
    ) -> Result<crate::models::JobPage> {
        use crate::models::JobsView;
        let per_page = per_page.clamp(5, 100);
        let page = page.max(1);

        let (scope, order) = match view {
            JobsView::Live => (
                "(status IN ('PENDING','RUNNING') OR (status IN ('COMPLETED','COMPLETED_MANUAL') AND cleared_at IS NULL))",
                // Working first, then waiting in the order they will run,
                // then delivered, newest first.
                "CASE WHEN status = 'RUNNING' THEN 0 WHEN status = 'PENDING' THEN 1 ELSE 2 END, \
                 CASE WHEN status IN ('RUNNING','PENDING') THEN -priority ELSE 0 END, \
                 CASE WHEN status IN ('RUNNING','PENDING') THEN created_at END ASC, \
                 COALESCE(completed_at, updated_at) DESC, id DESC",
            ),
            JobsView::Review => (
                "status IN ('REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED')",
                "updated_at DESC, id DESC",
            ),
            JobsView::Completed => (
                "status IN ('COMPLETED','COMPLETED_MANUAL')",
                "COALESCE(completed_at, updated_at) DESC, id DESC",
            ),
        };

        let mut clauses = vec![scope.to_string()];
        let mut values: Vec<String> = Vec::new();
        let search = filter.search.trim();
        if !search.is_empty() {
            let like = format!(
                "%{}%",
                search.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
            );
            clauses.push(
                "(slug LIKE ? ESCAPE '\\' OR url LIKE ? ESCAPE '\\' OR keyword LIKE ? ESCAPE '\\' OR journalist LIKE ? ESCAPE '\\')"
                    .into(),
            );
            values.extend(std::iter::repeat(like).take(4));
        }
        if !filter.journalist.trim().is_empty() {
            clauses.push("journalist = ?".into());
            values.push(filter.journalist.trim().to_uppercase());
        }
        match filter.group.trim() {
            "" => {}
            "-" => clauses.push("group_code IS NULL".into()),
            code => {
                clauses.push("group_code = ?".into());
                values.push(code.to_string());
            }
        }
        let where_sql = clauses.join(" AND ");

        let conn = self.pool.get()?;
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM queue WHERE {where_sql}"),
            rusqlite::params_from_iter(values.iter()),
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(&format!(
            "SELECT * FROM queue WHERE {where_sql} ORDER BY {order} LIMIT {per_page} OFFSET {}",
            (page - 1) * per_page
        ))?;
        let jobs = stmt
            .query_map(rusqlite::params_from_iter(values.iter()), Self::map_job_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(crate::models::JobPage { jobs, total, page, per_page })
    }

    /// Tab badge counts for the MCR desk, in one query.
    pub fn job_counts(&self) -> Result<crate::models::JobCounts> {
        let conn = self.pool.get()?;
        Ok(conn.query_row(
            r#"
            SELECT
              COALESCE(SUM(status IN ('PENDING','RUNNING')), 0),
              COALESCE(SUM(status IN ('COMPLETED','COMPLETED_MANUAL') AND cleared_at IS NULL), 0),
              COALESCE(SUM(status IN ('REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED')), 0),
              COALESCE(SUM(status IN ('COMPLETED','COMPLETED_MANUAL')), 0)
            FROM queue
            "#,
            [],
            |r| {
                Ok(crate::models::JobCounts {
                    active: r.get(0)?,
                    finished: r.get(1)?,
                    review: r.get(2)?,
                    completed: r.get(3)?,
                })
            },
        )?)
    }

    /// Take delivered jobs off the live queue. They are not deleted: the
    /// Completed list still has them. Returns how many were cleared.
    pub fn clear_finished_jobs(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        Ok(conn.execute(
            "UPDATE queue SET cleared_at = ? WHERE status IN ('COMPLETED','COMPLETED_MANUAL') AND cleared_at IS NULL",
            params![timestamps::now_string()],
        )?)
    }

    /// Run a delivered job again from its link (plan P7.1): the file was
    /// deleted, or the wrong video came out. Everything about the last run
    /// is reset; the new file is delivered next to the old one if that is
    /// still there (`_2`), never over it. `false` if the job is not a
    /// delivered one.
    pub fn redownload_job(&self, id: i64) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            r#"
            UPDATE queue
            SET status = 'PENDING', stage = 'QUEUED', progress = 0.0, speed = '0 Mbps', eta = '--:--',
                attempts = 0, error_message = NULL, error_code = NULL, not_before = NULL,
                lease_owner = NULL, lease_expires_at = NULL, stage_started_at = NULL,
                file_path = NULL, delivered_at = NULL, completed_at = NULL, cleared_at = NULL,
                updated_at = ?
            WHERE id = ? AND status IN ('COMPLETED','COMPLETED_MANUAL')
            "#,
            params![timestamps::now_string(), id],
        )?;
        Ok(n > 0)
    }

    pub fn retry_job(&self, id: i64, new_url: Option<&str>) -> Result<()> {
        let conn = self.pool.get()?;
        if let Some(url) = new_url {
            conn.execute(
                r#"
                UPDATE queue
                SET url = ?, status = 'PENDING', progress = 0.0, speed = '0 Mbps', eta = '--:--',
                    error_message = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                WHERE id = ?
                "#,
                params![url, id],
            )?;
        } else {
            conn.execute(
                r#"
                UPDATE queue
                SET status = 'PENDING', progress = 0.0, speed = '0 Mbps', eta = '--:--',
                    error_message = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
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
        let stage_str: String = row.get("stage").unwrap_or_else(|_| "QUEUED".to_string());

        // An unrecognised status means the row is corrupt or was written by a
        // newer build. Surfacing it as REQUIRES_REVIEW puts it in front of an
        // operator instead of hiding it behind a plausible state (defect D-21),
        // and the warning names the value so it is diagnosable.
        let status = JobStatus::parse(&status_str).unwrap_or_else(|| {
            tracing::warn!(
                job_id = row.get::<_, i64>("id").unwrap_or(-1),
                status = %status_str,
                "Unrecognised job status in database; treating as REQUIRES_REVIEW"
            );
            JobStatus::RequiresReview
        });

        Ok(Job {
            id: row.get("id")?,
            url: row.get("url")?,
            slug: row.get("slug")?,
            journalist: row.get("journalist")?,
            keyword: row.get("keyword")?,
            index_str: row.get("index_str")?,
            status,
            stage: JobStage::parse(&stage_str).unwrap_or(JobStage::Queued),
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

            url_normalized: row.get("url_normalized").ok().flatten(),
            attempts: row.get("attempts").unwrap_or(0),
            max_attempts: row.get("max_attempts").unwrap_or(3),
            lease_owner: row.get("lease_owner").ok().flatten(),
            lease_expires_at: timestamps::parse_opt(row.get("lease_expires_at").ok().flatten()),
            stage_started_at: timestamps::parse_opt(row.get("stage_started_at").ok().flatten()),
            not_before: timestamps::parse_opt(row.get("not_before").ok().flatten()),
            error_code: row.get("error_code").ok().flatten(),
            source_path: row.get("source_path").ok().flatten(),
            compliance_json: row.get("compliance_json").ok().flatten(),
            candidates_json: row.get("candidates_json").ok().flatten(),
            extraction_method: row.get("extraction_method").ok().flatten(),
            stage_timings_json: row.get("stage_timings_json").ok().flatten(),
            email_message_id: row.get("email_message_id").ok().flatten(),
            group_code: row.get("group_code").ok().flatten(),
            max_videos: row.get("max_videos").ok().flatten(),
            parent_job_id: row.get("parent_job_id").ok().flatten(),
            delivered_at: timestamps::parse_opt(row.get("delivered_at").ok().flatten()),
            completed_at: timestamps::parse_opt(row.get("completed_at").ok().flatten()),

            created_at: timestamps::parse_opt(row.get("created_at").ok()),
            updated_at: timestamps::parse_opt(row.get("updated_at").ok()),
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
                created_at: timestamps::parse_opt(row.get("created_at").ok()),
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
                created_at: timestamps::parse_opt(row.get("created_at").ok()),
            }))
        } else {
            Ok(None)
        }
    }

    /// Whether any active administrator exists.
    ///
    /// The `/setup` first-run window is open exactly while this is false, so it
    /// is asked on every request to that route rather than cached: an admin
    /// created through the CLI while the daemon runs must close the window
    /// immediately, without a restart.
    pub fn has_active_admin(&self) -> Result<bool> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE role = 'admin' AND is_active = 1",
            [],
            |r| r.get(0),
        )?;
        Ok(count > 0)
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
                created_at: timestamps::parse_opt(row.get("created_at").ok()),
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
            "UPDATE users SET password_hash = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
            params![hash, user_id],
        )?;
        // A password change that leaves the old sessions alive has revoked
        // nothing: whoever the change was meant to lock out is still logged in.
        let ended = self.delete_sessions_for_user(user_id)?;
        self.log_audit(
            "INFO",
            "AUTH",
            &format!(
                "Password updated for User #{} ({} session(s) ended)",
                user_id, ended
            ),
        )?;
        Ok(())
    }

    pub fn set_user_active_status(&self, user_id: i64, active: bool) -> Result<()> {
        let conn = self.pool.get()?;
        let val = if active { 1 } else { 0 };
        conn.execute("UPDATE users SET is_active = ? WHERE id = ?", params![val, user_id])?;
        Ok(())
    }

    pub fn create_session(&self, user_id: i64, token: &str, expires_in_days: i64) -> Result<()> {
        self.create_session_for(user_id, token, Duration::days(expires_in_days))
    }

    /// Create a session recording where it came from (plan P2.2).
    ///
    /// The IP and user agent exist so that "end other sessions" and the audit
    /// trail can answer *which* session, from *where* — not for access control.
    /// Nothing authenticates on them.
    pub fn create_session_with_meta(
        &self,
        user_id: i64,
        token: &str,
        lifetime: Duration,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = Utc::now();
        conn.execute(
            "INSERT INTO sessions (token, user_id, expires_at, created_at, last_seen_at, ip, user_agent)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                token,
                user_id,
                timestamps::format(now + lifetime),
                timestamps::format(now),
                timestamps::format(now),
                ip,
                user_agent.map(|ua| ua.chars().take(256).collect::<String>()),
            ],
        )?;
        Ok(())
    }

    /// Refresh `last_seen_at` and report whether the session is idle-expired.
    ///
    /// Returns `false` when the session has not been used within `idle`, in
    /// which case it is deleted. The absolute expiry is still enforced by
    /// [`Self::get_user_by_session_token`]; this is the second, shorter clock.
    pub fn touch_session(&self, token: &str, idle: Duration) -> Result<bool> {
        let conn = self.pool.get()?;
        let last_seen: Option<String> = conn
            .query_row(
                "SELECT last_seen_at FROM sessions WHERE token = ?",
                params![token],
                |r| r.get(0),
            )
            .optional()?;

        let Some(last_seen) = last_seen else {
            return Ok(false);
        };

        // A row backfilled by migration 3 (or written before it) has an empty
        // string here. Treat that as "seen now" rather than as expired: the
        // upgrade must not log the newsroom out.
        if !last_seen.is_empty() {
            if let Some(seen) = timestamps::parse_opt(Some(last_seen)) {
                if Utc::now() - seen > idle {
                    conn.execute("DELETE FROM sessions WHERE token = ?", params![token])?;
                    return Ok(false);
                }
            }
        }

        conn.execute(
            "UPDATE sessions SET last_seen_at = ? WHERE token = ?",
            params![timestamps::format(Utc::now()), token],
        )?;
        Ok(true)
    }

    /// End every session for a user. Used by "sign out everywhere" and after a
    /// password change — a changed password that leaves old sessions alive has
    /// not actually revoked anything.
    pub fn delete_sessions_for_user(&self, user_id: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute("DELETE FROM sessions WHERE user_id = ?", params![user_id])?;
        Ok(n)
    }

    /// Drop sessions that are past their absolute expiry. Called at start-up
    /// and by the nightly maintenance tick.
    pub fn purge_expired_sessions(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "DELETE FROM sessions WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            [],
        )?;
        Ok(n)
    }

    /// Record a login attempt, successful or not (plan P2.2).
    pub fn record_login_attempt(
        &self,
        email: Option<&str>,
        ip: Option<&str>,
        successful: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO login_attempts (at, email, ip, successful, reason) VALUES (?, ?, ?, ?, ?)",
            params![
                timestamps::format(Utc::now()),
                email.map(|e| e.to_lowercase()),
                ip,
                if successful { 1 } else { 0 },
                reason
            ],
        )?;
        Ok(())
    }

    /// Most recent login attempts, newest first — the data behind the admin
    /// panel's "recent sign-ins" list.
    pub fn recent_login_attempts(&self, limit: i64) -> Result<Vec<LoginAttempt>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, at, email, ip, successful, reason FROM login_attempts
             ORDER BY id DESC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(LoginAttempt {
                id: row.get("id")?,
                at: timestamps::parse_opt(row.get("at").ok()),
                email: row.get("email")?,
                ip: row.get("ip")?,
                successful: row.get::<_, i64>("successful")? == 1,
                reason: row.get("reason")?,
            })
        })?;
        let mut list = Vec::new();
        for a in rows {
            list.push(a?);
        }
        Ok(list)
    }

    /// Move a session's `last_seen_at` back in time.
    ///
    /// Test-only, but it lives here because it is the only honest way to
    /// exercise the idle timeout: the alternative is a test that sleeps for
    /// hours or one that asserts against the clock arithmetic rather than
    /// against the query.
    #[doc(hidden)]
    pub fn backdate_session_last_seen(&self, token: &str, by: Duration) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE sessions SET last_seen_at = ? WHERE token = ?",
            params![timestamps::format(Utc::now() - by), token],
        )?;
        Ok(())
    }

    // ==========================================
    // Scheduled maintenance (plan P6.6)
    // ==========================================

    /// Make sure every task this build knows about has a row.
    ///
    /// A task added in a later version gets its first `next_run` here. It is
    /// scheduled *forward*, not immediately: an upgrade that ran every
    /// maintenance task at once — including a VACUUM — the moment the daemon
    /// came back would be a surprising thing for an upgrade to do.
    pub fn ensure_scheduled_tasks(&self, tasks: &[crate::scheduler::TaskSpec]) -> Result<()> {
        let conn = self.pool.get()?;
        let now = Utc::now();
        for task in tasks {
            conn.execute(
                "INSERT OR IGNORE INTO scheduled_tasks (name, enabled, next_run)
                 VALUES (?, 1, ?)",
                params![task.name, timestamps::format(task.cadence.next_after(now))],
            )?;
        }
        Ok(())
    }

    /// Tasks whose time has come.
    ///
    /// A task whose `next_run` passed while the machine was off is returned at
    /// the next tick rather than waiting for tomorrow — the old hourly
    /// `hour == "03"` check simply lost that day's run.
    pub fn due_tasks(&self) -> Result<Vec<String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT name FROM scheduled_tasks
             WHERE enabled = 1 AND next_run IS NOT NULL AND next_run <= ?
             ORDER BY next_run ASC",
        )?;
        let rows = stmt.query_map(params![timestamps::format(Utc::now())], |r| r.get(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Claim a task for this process.
    ///
    /// Pushes `next_run` forward *before* the work starts, so a task that
    /// takes longer than the tick interval is not started again on the next
    /// tick — and so a task that panics does not spin. `false` means someone
    /// else got there first.
    pub fn claim_task(
        &self,
        name: &str,
        next_run: chrono::DateTime<Utc>,
    ) -> Result<bool> {
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE scheduled_tasks SET next_run = ?
             WHERE name = ? AND enabled = 1 AND next_run IS NOT NULL AND next_run <= ?",
            params![
                timestamps::format(next_run),
                name,
                timestamps::format(Utc::now())
            ],
        )?;
        Ok(changed == 1)
    }

    /// Record how a run went.
    pub fn record_task_run(
        &self,
        name: &str,
        outcome: crate::scheduler::TaskOutcome,
        error: Option<&str>,
        elapsed_ms: i64,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE scheduled_tasks
                SET last_run = ?, last_outcome = ?, last_error = ?, last_ms = ?
              WHERE name = ?",
            params![
                timestamps::format(Utc::now()),
                outcome.as_str(),
                error.map(|e| e.chars().take(500).collect::<String>()),
                elapsed_ms,
                name
            ],
        )?;
        Ok(())
    }

    /// Schedule a task to run at the next tick — the admin panel's "Run now".
    pub fn run_task_now(&self, name: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE scheduled_tasks SET next_run = ?, enabled = 1 WHERE name = ?",
            params![timestamps::format(Utc::now() - Duration::seconds(1)), name],
        )?;
        Ok(changed == 1)
    }

    /// Every task, for the maintenance panel.
    pub fn list_scheduled_tasks(
        &self,
        specs: &[crate::scheduler::TaskSpec],
    ) -> Result<Vec<crate::scheduler::TaskStatus>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT name, enabled, next_run, last_run, last_outcome, last_error, last_ms
               FROM scheduled_tasks",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::scheduler::TaskStatus {
                name: row.get(0)?,
                description: String::new(),
                enabled: row.get::<_, i64>(1)? == 1,
                next_run: timestamps::parse_opt(row.get(2).ok()),
                last_run: timestamps::parse_opt(row.get(3).ok()),
                last_outcome: row.get(4)?,
                last_error: row.get(5)?,
                last_ms: row.get(6)?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            let mut status = r?;
            // The description lives in code, not in the database, so editing
            // the wording does not need a migration.
            if let Some(spec) = specs.iter().find(|s| s.name == status.name) {
                status.description = spec.description.to_string();
            }
            out.push(status);
        }
        // Stable order for the panel.
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Delete login attempts older than `days`.
    pub fn purge_login_attempts(&self, days: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let cutoff = timestamps::format(Utc::now() - Duration::days(days));
        let n = conn.execute("DELETE FROM login_attempts WHERE at < ?", params![cutoff])?;
        Ok(n)
    }

    /// Create a session with an arbitrary lifetime.
    ///
    /// `expires_at` is stored in the same RFC3339 format the expiry query
    /// compares against. The previous code wrote `to_rfc3339()`
    /// (`2026-09-19T16:00:00+00:00`) and compared it with `CURRENT_TIMESTAMP`
    /// (`2026-09-19 17:00:00`); SQLite compares those as text and `'T'` (0x54)
    /// sorts above `' '` (0x20), so any session expiring *earlier the same day*
    /// still authenticated.
    pub fn create_session_for(&self, user_id: i64, token: &str, lifetime: Duration) -> Result<()> {
        self.create_session_with_meta(user_id, token, lifetime, None, None)
    }

    pub fn get_user_by_session_token(&self, token: &str) -> Result<Option<User>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT u.*
            FROM users u
            JOIN sessions s ON u.id = s.user_id
            WHERE s.token = ? AND s.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') AND u.is_active = 1
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
                created_at: timestamps::parse_opt(row.get("created_at").ok()),
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
        let mut memberships: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT jg.journalist_id, g.code FROM journalist_groups jg JOIN groups g ON g.id = jg.group_id
                 ORDER BY jg.journalist_id, jg.position, g.code",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, code) = row?;
                memberships.entry(id).or_default().push(code);
            }
        }
        let mut stmt = conn.prepare("SELECT * FROM journalists ORDER BY surname ASC")?;
        let rows = stmt.query_map([], |row| {
            let emails_raw: String = row.get("emails")?;
            let emails: Vec<String> = serde_json::from_str(&emails_raw).unwrap_or_default();
            let aliases_raw: String = row.get("aliases")?;
            let aliases: Vec<String> = serde_json::from_str(&aliases_raw).unwrap_or_default();
            let id: i64 = row.get("id")?;
            Ok(Journalist {
                id,
                surname: row.get("surname")?,
                full_name: row.get("full_name")?,
                emails,
                default_priority: row.get("default_priority")?,
                aliases,
                groups: memberships.get(&id).cloned().unwrap_or_default(),
                created_at: timestamps::parse_opt(row.get("created_at").ok()),
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

    /// Replace a journalist's parser aliases (plan P4.3). Blank entries are
    /// dropped; a missing journalist is an error, not a silent no-op.
    pub fn set_journalist_aliases(&self, surname: &str, aliases: &[String]) -> Result<()> {
        let clean: Vec<String> = aliases
            .iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE journalists SET aliases = ? WHERE surname = ?",
            params![serde_json::to_string(&clean)?, surname.to_uppercase()],
        )?;
        if n == 0 {
            anyhow::bail!("no journalist named {surname}");
        }
        Ok(())
    }

    pub fn delete_journalist(&self, surname: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM journalists WHERE surname = ?", params![surname.to_uppercase()])?;
        Ok(())
    }

    // ==========================================
    // Taxonomy: groups and membership (plan P4.17)
    // ==========================================

    /// The directory the database lives in (`data/`), for files that belong
    /// with it: taxonomy backups, the seed files.
    pub fn data_dir(&self) -> Option<std::path::PathBuf> {
        self.db_path.as_deref().and_then(|p| p.parent()).map(|p| p.to_path_buf())
    }

    /// Write the current taxonomy to `data/backups/taxonomy/` (plan P4.27)
    /// and keep the newest [`TAXONOMY_BACKUPS_KEPT`]. `reason` is a short
    /// word in the file name: "manual", "before-import", "before-restore",
    /// "before-delete".
    pub fn backup_taxonomy(&self, reason: &str) -> Result<TaxonomyBackup> {
        let dir = self
            .data_dir()
            .ok_or_else(|| anyhow::anyhow!("no data directory for backups"))?
            .join("backups")
            .join("taxonomy");
        std::fs::create_dir_all(&dir)?;
        let reason: String = reason
            .chars()
            .filter(|c| c.is_ascii_lowercase() || *c == '-')
            .take(20)
            .collect();
        // Milliseconds: two backups in one second (an import right after a
        // manual one) must not overwrite each other.
        let stamp = Utc::now().format("%Y%m%dT%H%M%S%3fZ");
        let name = format!("taxonomy-{stamp}-{}.json", if reason.is_empty() { "manual" } else { &reason });
        let t = self.export_taxonomy()?;
        let body = serde_json::to_string_pretty(&t)? + "\n";
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, dir.join(&name))?;
        let mut all = self.list_taxonomy_backups()?;
        while all.len() > TAXONOMY_BACKUPS_KEPT {
            if let Some(oldest) = all.pop() {
                let _ = std::fs::remove_file(dir.join(&oldest.name));
            }
        }
        Ok(TaxonomyBackup {
            name,
            bytes: body.len() as u64,
            groups: t.groups.len(),
            people: t.people.len(),
        })
    }

    /// Taxonomy backups, newest first.
    pub fn list_taxonomy_backups(&self) -> Result<Vec<TaxonomyBackup>> {
        let Some(dir) = self.data_dir().map(|d| d.join("backups").join("taxonomy")) else {
            return Ok(Vec::new());
        };
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !valid_backup_name(&name) {
                continue;
            }
            let raw = std::fs::read_to_string(entry.path()).unwrap_or_default();
            let (groups, people) = serde_json::from_str::<Taxonomy>(&raw)
                .map(|t| (t.groups.len(), t.people.len()))
                .unwrap_or((0, 0));
            out.push(TaxonomyBackup {
                bytes: raw.len() as u64,
                name,
                groups,
                people,
            });
        }
        out.sort_by(|a, b| b.name.cmp(&a.name));
        Ok(out)
    }

    /// One backup's contents. The name is checked, so no other file can be read.
    pub fn read_taxonomy_backup(&self, name: &str) -> Result<Taxonomy> {
        if !valid_backup_name(name) {
            anyhow::bail!("not a taxonomy backup name: {name}");
        }
        let dir = self.data_dir().ok_or_else(|| anyhow::anyhow!("no data directory"))?;
        let raw = std::fs::read_to_string(dir.join("backups").join("taxonomy").join(name))
            .with_context(|| format!("no backup {name}"))?;
        Ok(serde_json::from_str(&raw).with_context(|| format!("backup {name} is not a taxonomy file"))?)
    }

    /// Put a backup back exactly: groups and people it does not list are
    /// removed. The state before is backed up first.
    pub fn restore_taxonomy_backup(&self, name: &str) -> Result<ImportReport> {
        let t = self.read_taxonomy_backup(name)?;
        self.backup_taxonomy("before-restore")?;
        self.import_taxonomy(&t, true)
    }

    pub fn list_groups(&self) -> Result<Vec<Group>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT code, name, kind, keywords, description FROM groups ORDER BY code")?;
        let rows = stmt.query_map([], |r| {
            let keywords: String = r.get(3)?;
            Ok(Group {
                code: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                keywords: serde_json::from_str(&keywords).unwrap_or_default(),
                description: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert or update one group by code. The group is normalised and
    /// validated first; an invalid one is an error naming the problem.
    pub fn save_group(&self, group: &Group) -> Result<Group> {
        let g = group.normalized();
        g.validate().map_err(anyhow::Error::msg)?;
        let conn = self.pool.get()?;
        upsert_group(&conn, &g)?;
        Ok(g)
    }

    /// Delete a group; its memberships go with it (foreign-key cascade).
    /// Jobs keep the label they were given.
    pub fn delete_group(&self, code: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        Ok(conn.execute("DELETE FROM groups WHERE code = ?", params![code.trim().to_uppercase()])? == 1)
    }

    /// Replace a journalist's groups; the first becomes the default. Every
    /// code must exist.
    pub fn set_journalist_groups(&self, surname: &str, codes: &[String]) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        replace_memberships(&tx, &surname.trim().to_uppercase(), codes)?;
        tx.commit()?;
        Ok(())
    }

    /// The whole taxonomy, for taxonomy.json.
    pub fn export_taxonomy(&self) -> Result<Taxonomy> {
        let people = self
            .list_journalists()?
            .into_iter()
            .filter(|j| j.surname != "MCR")
            .map(|j| Person {
                surname: j.surname,
                full_name: j.full_name,
                emails: j.emails,
                aliases: j.aliases,
                default_priority: j.default_priority,
                groups: j.groups,
            })
            .collect();
        Ok(Taxonomy {
            version: crate::taxonomy::TAXONOMY_VERSION,
            groups: self.list_groups()?,
            people,
        })
    }

    /// Import taxonomy.json in one transaction: all of it or none of it.
    ///
    /// Groups and people in the file are inserted or updated. With
    /// `replace`, groups and people *not* in the file are deleted too (MCR
    /// always stays). A person's groups are replaced by the file's list.
    /// Invalid input is refused with every problem listed.
    pub fn import_taxonomy(&self, input: &Taxonomy, replace: bool) -> Result<ImportReport> {
        let known: Vec<String> = if replace {
            Vec::new()
        } else {
            self.list_groups()?.into_iter().map(|g| g.code).collect()
        };
        let t = input.checked(&known).map_err(|errors| anyhow::anyhow!("taxonomy refused:\n- {}", errors.join("\n- ")))?;
        if t.people.iter().any(|p| p.surname == "MCR") {
            anyhow::bail!("taxonomy refused:\n- MCR is built in and cannot be imported");
        }

        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let mut report = ImportReport::default();
        for g in &t.groups {
            upsert_group(&tx, g)?;
            report.groups_saved += 1;
        }
        for p in &t.people {
            tx.execute(
                r#"
                INSERT INTO journalists (surname, full_name, emails, default_priority, aliases)
                VALUES (?, ?, ?, ?, ?)
                ON CONFLICT(surname) DO UPDATE SET
                    full_name = excluded.full_name,
                    emails = excluded.emails,
                    default_priority = excluded.default_priority,
                    aliases = excluded.aliases
                "#,
                params![
                    p.surname,
                    p.full_name,
                    serde_json::to_string(&p.emails)?,
                    p.default_priority,
                    serde_json::to_string(&p.aliases)?
                ],
            )?;
            replace_memberships(&tx, &p.surname, &p.groups)?;
            report.people_saved += 1;
        }
        if replace {
            let keep_groups: Vec<&str> = t.groups.iter().map(|g| g.code.as_str()).collect();
            for code in self.list_groups_in(&tx)? {
                if !keep_groups.contains(&code.as_str()) {
                    tx.execute("DELETE FROM groups WHERE code = ?", params![code])?;
                    report.groups_removed += 1;
                }
            }
            let keep_people: Vec<&str> = t.people.iter().map(|p| p.surname.as_str()).collect();
            let surnames: Vec<String> = {
                let mut stmt = tx.prepare("SELECT surname FROM journalists WHERE surname <> 'MCR'")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for s in surnames {
                if !keep_people.contains(&s.as_str()) {
                    tx.execute("DELETE FROM journalists WHERE surname = ?", params![s])?;
                    report.people_removed += 1;
                }
            }
        }
        tx.commit()?;
        Ok(report)
    }

    fn list_groups_in(&self, conn: &rusqlite::Connection) -> Result<Vec<String>> {
        let mut stmt = conn.prepare("SELECT code FROM groups")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// First start with a `data/taxonomy.json` and no groups yet: import it
    /// (merge). A bad file is logged and skipped; the daemon still starts.
    fn seed_taxonomy_from_file(&self, conn: &rusqlite::Connection) -> Result<()> {
        let groups: i64 = conn.query_row("SELECT COUNT(*) FROM groups", [], |r| r.get(0))?;
        if groups > 0 {
            return Ok(());
        }
        let Some(path) = self.db_path.as_deref().and_then(|p| p.parent()).map(|d| d.join("taxonomy.json")) else {
            return Ok(());
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(());
        };
        let parsed: Taxonomy = match serde_json::from_str(&raw) {
            Ok(t) => t,
            Err(e) => {
                warn!("Ignoring {:?}: not a taxonomy file ({e})", path);
                return Ok(());
            }
        };
        match self.import_taxonomy(&parsed, false) {
            Ok(r) => info!("Seeded {} groups and {} people from {:?}", r.groups_saved, r.people_saved, path),
            Err(e) => warn!("Ignoring {:?}: {e:#}", path),
        }
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

    /// Hand a job the source file it will be built from and release it to the
    /// workers (plan P4.6: a video attached to an email).
    ///
    /// Only a job still parked as MANUAL_DOWNLOAD moves: the watcher parks
    /// attachment jobs there while the file downloads, so no worker can lease
    /// one before its source exists. If MCR has already acted on the job,
    /// this changes nothing and returns `false`.
    pub fn attach_source(&self, id: i64, source_path: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let n = conn.execute(
            "UPDATE queue SET source_path = ?, status = 'PENDING', stage = 'QUEUED', updated_at = ?
             WHERE id = ? AND status = 'MANUAL_DOWNLOAD'",
            params![source_path, now, id],
        )?;
        if n == 1 {
            conn.execute(
                "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, 'QUEUED', 'INFO', ?)",
                params![id, now, "Attachment saved from the email; queued"],
            )?;
        }
        Ok(n == 1)
    }

    // ==========================================
    // Processed mail (plan P4.2, defect E-07)
    // ==========================================

    pub fn get_processed_mail(&self, internet_message_id: &str) -> Result<Option<ProcessedMail>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row(
                "SELECT * FROM processed_mail WHERE internet_message_id = ?",
                params![internet_message_id],
                Self::map_processed_mail,
            )
            .optional()?)
    }

    fn map_processed_mail(row: &rusqlite::Row) -> rusqlite::Result<ProcessedMail> {
        let list = |col: &str| -> Vec<String> {
            row.get::<_, Option<String>>(col)
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default()
        };
        Ok(ProcessedMail {
            internet_message_id: row.get("internet_message_id")?,
            source_id: row.get("source_id")?,
            processed_at: timestamps::parse_opt(row.get("processed_at").ok()),
            outcome: row.get("outcome")?,
            from_address: row.get("from_address")?,
            subject: row.get("subject")?,
            jobs_json: row.get("jobs_json")?,
            received_at: timestamps::parse_opt(row.get("received_at").ok().flatten()),
            from_name: row.get("from_name").ok().flatten(),
            to: list("to_addrs"),
            cc: list("cc_addrs"),
            body_text: row.get("body_text").ok().flatten(),
            attachments_json: row.get("attachments_json").ok().flatten(),
            parse_json: row.get("parse_json").ok().flatten(),
        })
    }

    /// Record (or update) what a message produced. Called only after its
    /// jobs are in the queue, so a row here means "nothing left to do".
    /// Where the last poll of `mailbox` got to (plan P4.8). `None` before the
    /// first poll that saw a message.
    pub fn get_mail_checkpoint(&self, mailbox: &str) -> Result<Option<chrono::DateTime<Utc>>> {
        let conn = self.pool.get()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT modified_since FROM mail_checkpoints WHERE mailbox = ?",
                params![mailbox],
                |row| row.get(0),
            )
            .optional()?;
        Ok(timestamps::parse_opt(raw))
    }

    pub fn set_mail_checkpoint(&self, mailbox: &str, modified_since: chrono::DateTime<Utc>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            r#"
            INSERT INTO mail_checkpoints (mailbox, modified_since, updated_at) VALUES (?, ?, ?)
            ON CONFLICT(mailbox) DO UPDATE SET
                modified_since = excluded.modified_since,
                updated_at     = excluded.updated_at
            "#,
            params![mailbox, timestamps::format(modified_since), timestamps::now_string()],
        )?;
        Ok(())
    }

    /// The newest handled mail first, for the admin panel and `mail-history`.
    // ------------------------------------------------------------------
    // Self-check links (plan P6.7)
    // ------------------------------------------------------------------

    pub fn list_selfcheck_links(&self) -> Result<Vec<crate::models::SelfcheckLink>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM selfcheck_links ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::models::SelfcheckLink {
                id: row.get("id")?,
                label: row.get("label")?,
                url: row.get("url")?,
                last_checked_at: timestamps::parse_opt(row.get("last_checked_at").ok()),
                last_ok: row.get::<_, Option<i64>>("last_ok")?.map(|v| v != 0),
                last_detail: row.get("last_detail")?,
                last_ok_at: timestamps::parse_opt(row.get("last_ok_at").ok()),
                failing_since: timestamps::parse_opt(row.get("failing_since").ok()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Add a link to the self-check. A link already there is an error, so an
    /// operator is told rather than seeing nothing happen.
    pub fn add_selfcheck_link(&self, label: &str, url: &str) -> Result<i64> {
        let conn = self.pool.get()?;
        let exists: bool = conn
            .query_row("SELECT 1 FROM selfcheck_links WHERE url = ?", params![url], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if exists {
            anyhow::bail!("That link is already in the self-check.");
        }
        conn.execute("INSERT INTO selfcheck_links (label, url) VALUES (?, ?)", params![label, url])?;
        Ok(conn.last_insert_rowid())
    }

    pub fn delete_selfcheck_link(&self, id: i64) -> Result<bool> {
        let conn = self.pool.get()?;
        Ok(conn.execute("DELETE FROM selfcheck_links WHERE id = ?", params![id])? > 0)
    }

    /// Store one link's result. Returns the previous `last_ok`, so the caller
    /// can tell a link that just broke (or just recovered) from one that has
    /// been failing for a week.
    pub fn record_selfcheck_result(&self, id: i64, ok: bool, detail: &str) -> Result<Option<bool>> {
        let conn = self.pool.get()?;
        let previous: Option<Option<i64>> = conn
            .query_row("SELECT last_ok FROM selfcheck_links WHERE id = ?", params![id], |r| r.get(0))
            .optional()?;
        let now = timestamps::now_string();
        conn.execute(
            r#"
            UPDATE selfcheck_links
            SET last_checked_at = ?1,
                last_ok = ?2,
                last_detail = ?3,
                last_ok_at = CASE WHEN ?2 = 1 THEN ?1 ELSE last_ok_at END,
                failing_since = CASE
                    WHEN ?2 = 1 THEN NULL
                    WHEN failing_since IS NULL THEN ?1
                    ELSE failing_since END
            WHERE id = ?4
            "#,
            params![now, ok as i64, detail, id],
        )?;
        Ok(previous.flatten().map(|v| v != 0))
    }

    pub fn list_processed_mail(&self, limit: i64) -> Result<Vec<ProcessedMail>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM processed_mail ORDER BY processed_at DESC LIMIT ?")?;
        let rows = stmt.query_map(params![limit.clamp(1, 1000)], Self::map_processed_mail)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Ask the watcher to read a handled mail again (plan P4.25). Returns
    /// the mail's record, or an error when there is none or it has no
    /// provider id to fetch it by.
    pub fn request_mail_reprocess(&self, internet_message_id: &str, requested_by: &str) -> Result<ProcessedMail> {
        let m = self
            .get_processed_mail(internet_message_id)?
            .ok_or_else(|| anyhow::anyhow!("no handled mail with Message-ID {internet_message_id}"))?;
        let source = m
            .source_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("the mail's mailbox id was not recorded; it cannot be fetched again"))?;
        let conn = self.pool.get()?;
        conn.execute(
            r#"
            INSERT INTO mail_reprocess (internet_message_id, source_id, requested_at, requested_by)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(internet_message_id) DO UPDATE SET
                requested_at = excluded.requested_at,
                requested_by = excluded.requested_by,
                attempts = 0
            "#,
            params![m.internet_message_id, source, timestamps::now_string(), requested_by],
        )?;
        Ok(m)
    }

    /// Pending reprocess requests, oldest first: (Message-ID, provider id, attempts).
    pub fn pending_mail_reprocess(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.pool.get()?;
        let mut stmt =
            conn.prepare("SELECT internet_message_id, source_id, attempts FROM mail_reprocess ORDER BY requested_at")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn note_mail_reprocess_failure(&self, internet_message_id: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE mail_reprocess SET attempts = attempts + 1 WHERE internet_message_id = ?",
            params![internet_message_id],
        )?;
        Ok(())
    }

    pub fn finish_mail_reprocess(&self, internet_message_id: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM mail_reprocess WHERE internet_message_id = ?", params![internet_message_id])?;
        Ok(())
    }

    /// Forget that a mail was handled, so it is parsed as new. Only the
    /// reprocess path calls this; its jobs are untouched.
    pub fn forget_processed_mail(&self, internet_message_id: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM processed_mail WHERE internet_message_id = ?", params![internet_message_id])?;
        Ok(())
    }

    /// Record what a mail produced, and (plan P7.6) what it said.
    ///
    /// On a second record of the same mail the content columns are replaced
    /// only by a value: a FAILED record, which knows only the headers, must
    /// not erase a text an earlier attempt stored.
    pub fn record_processed_mail(&self, m: &ProcessedMail) -> Result<()> {
        let conn = self.pool.get()?;
        let list = |v: &Vec<String>| (!v.is_empty()).then(|| serde_json::to_string(v).unwrap_or_default());
        conn.execute(
            r#"
            INSERT INTO processed_mail
                (internet_message_id, source_id, processed_at, outcome, from_address, subject, jobs_json,
                 received_at, from_name, to_addrs, cc_addrs, body_text, attachments_json, parse_json)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(internet_message_id) DO UPDATE SET
                source_id        = excluded.source_id,
                processed_at     = excluded.processed_at,
                outcome          = excluded.outcome,
                jobs_json        = excluded.jobs_json,
                received_at      = COALESCE(excluded.received_at, received_at),
                from_name        = COALESCE(excluded.from_name, from_name),
                to_addrs         = COALESCE(excluded.to_addrs, to_addrs),
                cc_addrs         = COALESCE(excluded.cc_addrs, cc_addrs),
                body_text        = COALESCE(excluded.body_text, body_text),
                attachments_json = COALESCE(excluded.attachments_json, attachments_json),
                parse_json       = COALESCE(excluded.parse_json, parse_json)
            "#,
            params![
                m.internet_message_id,
                m.source_id,
                timestamps::now_string(),
                m.outcome,
                m.from_address,
                m.subject,
                m.jobs_json,
                m.received_at.map(timestamps::format),
                m.from_name.as_deref().filter(|s| !s.trim().is_empty()),
                list(&m.to),
                list(&m.cc),
                m.body_text,
                m.attachments_json,
                m.parse_json,
            ],
        )?;
        Ok(())
    }

    // ==========================================
    // MCR mail view (plan P7.7)
    // ==========================================

    /// Every mail received (or, for mail from before plan P7.6, handled)
    /// since `since`, with the start of its text.
    pub fn inbox_mail_rows(&self, since: chrono::DateTime<Utc>) -> Result<Vec<crate::models::InboxMailRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT internet_message_id, received_at, processed_at, outcome, from_address, from_name, subject,
                    substr(body_text, 1, 4000) AS body_head, attachments_json, parse_json, jobs_json
             FROM processed_mail
             WHERE COALESCE(received_at, processed_at) >= ?",
        )?;
        let rows = stmt.query_map(params![timestamps::format(since)], |r| {
            Ok(crate::models::InboxMailRow {
                internet_message_id: r.get("internet_message_id")?,
                received_at: timestamps::parse_opt(r.get("received_at").ok().flatten()),
                processed_at: timestamps::parse_opt(r.get("processed_at").ok().flatten()),
                outcome: r.get("outcome")?,
                from_address: r.get("from_address")?,
                from_name: r.get("from_name").ok().flatten(),
                subject: r.get("subject")?,
                body_head: r.get("body_head").ok().flatten(),
                attachments_json: r.get("attachments_json").ok().flatten(),
                parse_json: r.get("parse_json").ok().flatten(),
                jobs_json: r.get("jobs_json")?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The jobs created since `since`, and the ones in `extra_ids` whenever
    /// they were created (an older job a mail's duplicate link points at).
    pub fn inbox_job_rows(&self, since: chrono::DateTime<Utc>, extra_ids: &[i64]) -> Result<Vec<crate::models::InboxJobRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, url, slug, journalist, index_str, status, stage, progress, email_message_id,
                    parent_job_id, submitted_by_user_id, group_code, created_at
             FROM queue
             WHERE created_at >= ? OR id IN (SELECT value FROM json_each(?))",
        )?;
        let extra = serde_json::to_string(extra_ids)?;
        let rows = stmt.query_map(params![timestamps::format(since), extra], |r| {
            Ok(crate::models::InboxJobRow {
                id: r.get("id")?,
                url: r.get("url")?,
                slug: r.get("slug")?,
                journalist: r.get("journalist")?,
                index_str: r.get("index_str")?,
                status: r.get("status")?,
                stage: r.get::<_, Option<String>>("stage")?.unwrap_or_else(|| "QUEUED".into()),
                progress: r.get::<_, Option<f64>>("progress")?.unwrap_or(0.0),
                email_message_id: r.get("email_message_id")?,
                parent_job_id: r.get("parent_job_id")?,
                submitted_by_user_id: r.get("submitted_by_user_id")?,
                group_code: r.get("group_code")?,
                created_at: timestamps::parse_opt(r.get("created_at").ok().flatten()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A mail's own jobs (its Message-ID, article siblings included) and the
    /// jobs in `also` (those its duplicate links point at), oldest first.
    pub fn jobs_for_mail(&self, internet_message_id: &str, also: &[i64]) -> Result<Vec<Job>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM queue
             WHERE email_message_id = ? OR id IN (SELECT value FROM json_each(?))
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![internet_message_id, serde_json::to_string(also)?], Self::map_job_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A job added by hand and the videos found in its article.
    pub fn jobs_with_children(&self, id: i64) -> Result<Vec<Job>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM queue WHERE id = ? OR parent_job_id = ? ORDER BY id")?;
        let rows = stmt.query_map(params![id, id], Self::map_job_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Add one entry to a handled mail's `jobs_json`: a link MCR queued from
    /// the mail's text (plan P7.8), so the view files the job under it.
    /// `false` when the mail is not there.
    pub fn append_mail_job(&self, internet_message_id: &str, entry: &serde_json::Value) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE processed_mail SET jobs_json = json_insert(COALESCE(jobs_json, '[]'), '$[#]', json(?))
             WHERE internet_message_id = ?",
            params![entry.to_string(), internet_message_id],
        )?;
        Ok(n == 1)
    }

    /// The subjects of the mails `keys` (plan P7.11): a video on the desk
    /// says which mail it came from.
    pub fn mail_subjects(&self, keys: &[String]) -> Result<std::collections::HashMap<String, String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT internet_message_id, COALESCE(subject, '') FROM processed_mail
             WHERE internet_message_id IN (SELECT value FROM json_each(?))",
        )?;
        let rows = stmt.query_map(params![serde_json::to_string(keys)?], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forget the text of mail handled more than `older_than_days` ago
    /// (plan P7.6). The row stays: sender, subject and the jobs it produced
    /// are the record of what came in. Returns how many texts were cleared.
    pub fn purge_mail_text(&self, older_than_days: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let cutoff = timestamps::format(Utc::now() - Duration::days(older_than_days.max(1)));
        Ok(conn.execute(
            "UPDATE processed_mail SET body_text = NULL
             WHERE body_text IS NOT NULL AND COALESCE(received_at, processed_at) < ?",
            params![cutoff],
        )?)
    }

    // ==========================================
    // Audit Logging
    // ==========================================

    pub fn log_audit(&self, level: &str, category: &str, message: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO audit_logs (level, category, message, timestamp) VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
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
                timestamp: timestamps::parse_opt(row.get("timestamp").ok()),
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
    fn journalist_aliases_round_trip_and_survive_a_resave() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        repo.save_journalist("PAPADAKI", "Anna Papadaki", &[], 0)?;
        repo.set_journalist_aliases("papadaki", &["ΠΑΠΑΔΑΚΗ".into(), "  ".into(), " ΑΝΝΑΣ ".into()])?;

        let find = |repo: &Repository| -> Result<Vec<String>> {
            Ok(repo
                .list_journalists()?
                .into_iter()
                .find(|j| j.surname == "PAPADAKI")
                .unwrap()
                .aliases)
        };
        assert_eq!(find(&repo)?, vec!["ΠΑΠΑΔΑΚΗ".to_string(), "ΑΝΝΑΣ".to_string()]);

        // Editing name or addresses in the panel must not wipe the aliases.
        repo.save_journalist("PAPADAKI", "A. Papadaki", &["a@example.gr".into()], 5)?;
        assert_eq!(find(&repo)?.len(), 2);

        assert!(repo.set_journalist_aliases("NOBODY", &["X".into()]).is_err());
        Ok(())
    }

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

        let owner = "test-host:1:0";
        let leased = repo.lease_job(owner, 120)?.expect("Job must be leased");
        assert_eq!(leased.id, job_id);
        assert_eq!(leased.status, JobStatus::Running);
        assert_eq!(leased.stage, JobStage::Extract);

        // 4. Progress and Stage Updates
        repo.update_job_progress(job_id, 45.5, "12.4MB/s", "00:15")?;
        repo.set_stage(job_id, owner, JobStage::Transcode)?;

        let current_job = repo.get_job(job_id)?.expect("Job must exist");
        assert_eq!(current_job.status, JobStatus::Running);
        assert_eq!(current_job.stage, JobStage::Transcode);
        assert_eq!(current_job.progress, 45.5);

        // 5. Completion
        repo.finish(job_id, owner, JobStatus::Completed, None, None, Some("C:/Watchfolder/1_PAPADAKI_PARADE.mxf"))?;
        repo.update_job_status(job_id, JobStatus::Completed, None, None, Some(120.5))?;
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



/// How many taxonomy backups are kept (plan P4.27).
pub const TAXONOMY_BACKUPS_KEPT: usize = 30;

/// One file in `data/backups/taxonomy/`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TaxonomyBackup {
    pub name: String,
    pub bytes: u64,
    pub groups: usize,
    pub people: usize,
}

/// `taxonomy-20260930T134501123Z-before-import.json`, and nothing else: no
/// separators, so a name from a request can never leave the directory.
fn valid_backup_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("taxonomy-").and_then(|r| r.strip_suffix(".json")) else {
        return false;
    };
    rest.len() <= 60 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn upsert_group(conn: &rusqlite::Connection, g: &Group) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO groups (code, name, kind, keywords, description, created_at)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(code) DO UPDATE SET
            name = excluded.name,
            kind = excluded.kind,
            keywords = excluded.keywords,
            description = excluded.description
        "#,
        params![g.code, g.name, g.kind, serde_json::to_string(&g.keywords)?, g.description, timestamps::now_string()],
    )?;
    Ok(())
}

/// Replace one journalist's memberships with `codes`, in order.
fn replace_memberships(conn: &rusqlite::Connection, surname: &str, codes: &[String]) -> Result<()> {
    let jid: i64 = conn
        .query_row("SELECT id FROM journalists WHERE surname = ?", params![surname], |r| r.get(0))
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("no journalist named {surname}"))?;
    conn.execute("DELETE FROM journalist_groups WHERE journalist_id = ?", params![jid])?;
    for (pos, code) in codes.iter().enumerate() {
        let code = code.trim().to_uppercase();
        let gid: i64 = conn
            .query_row("SELECT id FROM groups WHERE code = ?", params![code], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("no group {code}"))?;
        conn.execute(
            "INSERT OR IGNORE INTO journalist_groups (journalist_id, group_id, position) VALUES (?, ?, ?)",
            params![jid, gid, pos as i64],
        )?;
    }
    Ok(())
}
