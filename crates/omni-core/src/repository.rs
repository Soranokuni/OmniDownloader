use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use rusqlite::OptionalExtension;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

use crate::auth::{hash_password, verify_password};
use crate::migrations;
use crate::models::{
    AuditLog, Enqueued, Job, JobEvent, JobStage, JobStatus, Journalist, LoginAttempt, NewJob,
    ProcessedMail, QueueSummary, QueueWatch, User,
    UserRole,
};
use crate::site_health::{self, SiteOutcome, SiteRow, SiteState};
use crate::taxonomy::{Group, ImportReport, Person, Taxonomy};
use crate::timestamps;

/// Default window for treating a re-sent link as already delivered.
///
/// A day covers the realistic case (the same story forwarded again during one
/// news cycle) without blocking a genuine re-ingest the next day.
pub const DEFAULT_DEDUP_WINDOW_HOURS: i64 = 24;

/// One page-evidence file of a failed job (plan P10.1).
#[derive(Debug, Clone)]
pub struct EvidenceRow {
    pub job_id: i64,
    pub path: String,
    pub bytes: i64,
    /// `None` when the stored text does not parse; never shown as "now".
    pub created_at: Option<chrono::DateTime<Utc>>,
}

/// Self-heal runs kept (P10.14).
pub const SELFHEAL_MAX_ROWS: i64 = 500;
/// Days a self-heal run is kept.
pub const SELFHEAL_KEEP_DAYS: i64 = 30;

/// One overnight self-heal run on a broken site (plan P10.14).
#[derive(Debug, Clone)]
pub struct SelfhealReport {
    pub id: i64,
    pub domain: String,
    /// `None` when the stored text does not parse; never shown as "now".
    pub at: Option<chrono::DateTime<Utc>>,
    /// `found` | `nothing` | `skipped` | `outage`.
    pub outcome: String,
    pub recipe_hash: Option<String>,
    pub origin: Option<String>,
    pub pages: i64,
    pub detail: Option<String>,
}

/// One site for the admin's Sites card (P10.15a): its health row and its canaries.
#[derive(Debug, Clone)]
pub struct SiteListRow {
    pub site: SiteRow,
    /// Canaries whose page is still there.
    pub canaries: usize,
    /// ...of which the last check passed.
    pub canaries_ok: usize,
    /// The newest canary check; `None` = never checked.
    pub canary_checked_at: Option<chrono::DateTime<Utc>>,
}

/// One (recipe, site) row of the admin's Recipes card.
#[derive(Debug, Clone)]
pub struct RecipeSiteView {
    pub domain: String,
    pub state: String,
    pub suspended_reason: Option<String>,
    pub rank: f64,
    pub approvals: i64,
    pub rejections: i64,
    pub uses: i64,
    pub last_used_at: Option<chrono::DateTime<Utc>>,
    pub last_ok_at: Option<chrono::DateTime<Utc>>,
}

/// One stored recipe with its per-site scores (P10.15a).
#[derive(Debug, Clone)]
pub struct RecipeListRow {
    pub id: i64,
    pub hash: String,
    pub scope_kind: String,
    pub scope_key: String,
    pub family: Option<String>,
    pub origin: String,
    pub state: String,
    pub created_at: Option<chrono::DateTime<Utc>>,
    pub retired_at: Option<chrono::DateTime<Utc>>,
    pub retired_reason: Option<String>,
    pub recipe_json: String,
    /// Score over all the sites it has a row on.
    pub rank: f64,
    pub sites: Vec<RecipeSiteView>,
}

/// A recipe for the admin's export file.
#[derive(Debug, Clone)]
pub struct RecipeExportRow {
    pub scope_kind: String,
    pub scope_key: String,
    pub family: Option<String>,
    pub recipe_json: String,
}

/// What an admin's retirement of a recipe did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminRetire {
    Retired,
    NotFound,
    /// A shipped recipe leaves with the program, never by hand.
    Shipped,
    AlreadyRetired,
}

/// A recipe an admin's import pushed out to make room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictedRecipe {
    pub id: i64,
    pub scope_kind: String,
    pub scope_key: String,
    /// `its scope was full` | `the store was full`.
    pub why: String,
}

/// What one entry of an import did, and whom it evicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeImportResult {
    pub outcome: crate::recipe_score::InsertOutcome,
    pub evicted: Vec<EvictedRecipe>,
}

/// An image distance up to this counts as «the article's picture is in the video» (P10.11b's card).
pub const IMAGE_MATCH_MAX: i64 = 10;

/// What the repair ladder did on one site in a period (P10.15a), the owner's
/// evidence for moving `repair.mode`. Every rate is `None` when nothing could
/// be measured (never a made-up 0 %).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SiteReport {
    /// `(unknown)` for runs that never found the site.
    pub domain: String,
    /// Ladder runs; the «Επόμενη επιλογή» rows are counted apart in `next_choices`.
    pub attempts: i64,
    /// MCR's «next choice» downloads.
    pub next_choices: i64,
    pub shadow_attempts: i64,
    pub hold_attempts: i64,
    /// Ended with a held video or, in shadow mode, one that would have been held.
    pub found: i64,
    pub held: i64,
    pub approved: i64,
    /// MCR's «Λάθος βίντεο».
    pub rejected_wrong_video: i64,
    /// Rejected for any other reason.
    pub rejected_other: i64,
    pub expired: i64,
    /// Found runs whose page stated a length (the gate has already dropped a video that did not match it).
    pub length_stated: i64,
    /// Found runs with an article-picture comparison, and those within [`IMAGE_MATCH_MAX`].
    pub image_known: i64,
    pub image_matched: i64,
    pub llm_calls: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub median_duration_ms: Option<i64>,
    /// Overnight / daytime self-heal runs on the site, and those that found a way.
    pub heal_runs: i64,
    pub heal_found: i64,
}

impl SiteReport {
    pub fn length_stated_rate(&self) -> Option<f64> {
        (self.found > 0).then(|| self.length_stated as f64 / self.found as f64)
    }

    pub fn image_match_rate(&self) -> Option<f64> {
        (self.image_known > 0).then(|| self.image_matched as f64 / self.image_known as f64)
    }
}

/// One attempt as the report reads it.
struct ReportRow {
    domain: Option<String>,
    mode: String,
    outcome: Option<String>,
    decision: Option<String>,
    rung: Option<String>,
    wrong_video: bool,
    expected_secs: Option<f64>,
    image_distance: Option<i64>,
    llm_calls: i64,
    tokens_in: i64,
    tokens_out: i64,
    duration_ms: Option<i64>,
}

/// Median of the values, the mean of the middle two when there is an even number.
fn median(mut v: Vec<i64>) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2 })
}

fn aggregate_report(rows: Vec<ReportRow>) -> Vec<SiteReport> {
    use std::collections::BTreeMap;
    let mut by: BTreeMap<String, (SiteReport, Vec<i64>)> = BTreeMap::new();
    for r in rows {
        let domain = r.domain.clone().filter(|d| !d.is_empty()).unwrap_or_else(|| "(unknown)".to_string());
        let (rep, durations) = by.entry(domain.clone()).or_insert_with(|| (SiteReport { domain, ..Default::default() }, Vec::new()));
        let next = r.rung.as_deref() == Some("next");
        if next {
            rep.next_choices += 1;
        } else {
            rep.attempts += 1;
            match r.mode.as_str() {
                "shadow" => rep.shadow_attempts += 1,
                "hold" => rep.hold_attempts += 1,
                _ => {}
            }
            durations.extend(r.duration_ms);
        }
        rep.llm_calls += r.llm_calls;
        rep.tokens_in += r.tokens_in;
        rep.tokens_out += r.tokens_out;
        let outcome = r.outcome.as_deref();
        let found = !next && matches!(outcome, Some("held" | "shadow_found"));
        if found {
            rep.found += 1;
            if r.expected_secs.is_some() {
                rep.length_stated += 1;
            }
            if let Some(d) = r.image_distance {
                rep.image_known += 1;
                if d <= IMAGE_MATCH_MAX {
                    rep.image_matched += 1;
                }
            }
        }
        if outcome == Some("held") {
            rep.held += 1;
        }
        match r.decision.as_deref() {
            Some("approved") => rep.approved += 1,
            Some("rejected") if r.wrong_video => rep.rejected_wrong_video += 1,
            Some("rejected") => rep.rejected_other += 1,
            Some("expired") => rep.expired += 1,
            _ => {}
        }
    }
    by.into_values()
        .map(|(mut rep, durations)| {
            rep.median_duration_ms = median(durations);
            rep
        })
        .collect()
}

/// One run of the repair ladder on a job (plan P10.11a).
#[derive(Debug, Clone)]
pub struct RepairAttempt {
    pub id: i64,
    pub job_id: i64,
    /// `None` when the stored text does not parse; never shown as "now".
    pub started_at: Option<chrono::DateTime<Utc>>,
    pub finished_at: Option<chrono::DateTime<Utc>>,
    /// `shadow` | `hold`.
    pub mode: String,
    pub rung: Option<String>,
    pub outcome: Option<String>,
    /// The full `recipes.hash` behind the top candidate.
    pub recipe_hash: Option<String>,
    pub recipe_id: Option<i64>,
    pub domain: Option<String>,
    pub llm_calls: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// The verified candidates with their checks, in rank order.
    pub candidates_json: Option<String>,
    pub next_index: i64,
    pub duration_ms: Option<i64>,
    /// The exact address that was held, when the attempt held one.
    pub held_url: Option<String>,
    /// What MCR decided about the held video: approved | rejected | expired.
    pub mcr_decision: Option<String>,
    /// dHash distance between the article picture and the closest preview frame (P10.11b); `None` = no comparison.
    pub image_distance: Option<i64>,
    /// Time of that frame in the preview.
    pub image_frame_secs: Option<f64>,
    /// The host the article picture came from.
    pub image_host: Option<String>,
    /// The length the page itself stated for its video.
    pub expected_secs: Option<f64>,
}

/// What a repair attempt concluded, for [`Repository::finish_attempt`].
#[derive(Debug, Clone, Default)]
pub struct AttemptEnd {
    pub rung: Option<String>,
    pub outcome: Option<String>,
    pub recipe_hash: Option<String>,
    pub recipe_id: Option<i64>,
    pub domain: Option<String>,
    pub candidates_json: Option<String>,
    pub duration_ms: Option<i64>,
    pub held_url: Option<String>,
    /// Place of the held video in `candidates_json`.
    pub next_index: i64,
    pub expected_secs: Option<f64>,
    /// Model calls this run made, and the tokens they used (P10.12).
    pub llm_calls: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
}

/// A finished video held back until MCR approves it (plan P10.5).
#[derive(Debug, Clone)]
pub struct HeldOutput {
    pub job_id: i64,
    pub path: String,
    pub bytes: i64,
    /// Lower-case hex SHA-256 of the file as held; approval names it and
    /// delivery re-checks it.
    pub sha256: String,
    pub duration: Option<f64>,
    /// `None` when the stored text does not parse; never shown as "now".
    pub created_at: Option<chrono::DateTime<Utc>>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
    /// `None` while waiting, then approved | rejected | expired | superseded | delivered.
    pub decision: Option<String>,
    pub decided_by: Option<String>,
    pub decided_at: Option<chrono::DateTime<Utc>>,
    pub reason: Option<String>,
    /// Where the approved file was copied in the watchfolder; set together
    /// with the job's `file_path` right after the copy, so a restart can tell
    /// that THIS file was delivered.
    pub delivered_path: Option<String>,
}

/// The first-run administrator of a new installation (see
/// [`Repository::ensure_default_admin`]). Logins are matched lowercased, so
/// "Admin" and "admin" both work.
pub const DEFAULT_ADMIN_LOGIN: &str = "admin";
pub const DEFAULT_ADMIN_PASSWORD: &str = "Admin";

/// The start of today in the server's local time, as UTC.
fn local_midnight_utc() -> chrono::DateTime<Utc> {
    use chrono::{Local, TimeZone};
    let midnight = Local::now().date_naive().and_hms_opt(0, 0, 0);
    midnight
        .and_then(|m| Local.from_local_datetime(&m).earliest())
        .map(|d| d.with_timezone(&Utc))
        // A midnight that does not exist locally: fall back to 24 h ago.
        .unwrap_or_else(|| Utc::now() - Duration::hours(24))
}

/// What [`Repository::cancel_job`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Cancelled,
    /// The job is already delivering; the file cannot be stopped.
    TooLate,
    /// Not waiting or running (or no such job).
    NotCancellable,
}

/// The stage clock of a leased job, as the new `stage_timings_json` text: the
/// existing object with the milliseconds since `stage_started_at` added to the
/// key of the current stage (repeats of a stage sum). `None` when there is
/// nothing to write: no such leased job, no start time, or no stage. Malformed
/// stored JSON starts a fresh object, since a stage change must not fail over
/// it. Negative elapsed time (a clock step) counts as 0.
fn stage_timings_for(
    conn: &rusqlite::Connection,
    guard: &str,
    args: impl rusqlite::Params,
    at: chrono::DateTime<Utc>,
) -> Result<Option<String>> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
        .query_row(
            &format!("SELECT stage, stage_started_at, stage_timings_json FROM queue WHERE {guard}"),
            args,
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((Some(stage), Some(started), existing)) = row else {
        return Ok(None);
    };
    let Some(started) = timestamps::parse(&started) else {
        return Ok(None);
    };
    if stage.is_empty() {
        return Ok(None);
    }
    let ms = (at - started).num_milliseconds().max(0);
    let mut map = match existing.as_deref().map(serde_json::from_str::<serde_json::Value>) {
        Some(Ok(serde_json::Value::Object(m))) => m,
        _ => serde_json::Map::new(),
    };
    let prev = map.get(&stage).and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    map.insert(stage, serde_json::Value::from(prev.saturating_add(ms)));
    Ok(Some(serde_json::Value::Object(map).to_string()))
}

/// What [`Repository::cancel_own_pending`] did. A job that is not the
/// caller's is `NotFound`, the same as one that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnCancelOutcome {
    Cancelled,
    /// No such job, or it belongs to someone else.
    NotFound,
    /// The caller's own job, but no longer waiting.
    NotCancellable,
}

#[derive(Clone)]
pub struct Repository {
    pool: Arc<Pool<SqliteConnectionManager>>,
    /// Used for the pre-migration backup; `None` for in-memory test databases.
    db_path: Option<Arc<Path>>,
    /// Live-update bus; `None` in tests and tools that nobody watches.
    events: Option<crate::events::EventBus>,
    /// The log redactor: every cookie jar read joins it, so a jar written by
    /// another process (the sign-in command) is redacted from its first use.
    redactions: Option<crate::logging::Redactions>,
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
            events: None,
            redactions: None,
        };
        repo.run_migrations()?;
        Ok(repo)
    }

    /// Announce job changes on `bus` after they are written (P1.10).
    pub fn with_events(mut self, bus: crate::events::EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    /// Register the values of every cookie jar this repository opens with `r`.
    pub fn with_cookie_redactions(mut self, r: crate::logging::Redactions) -> Self {
        self.redactions = Some(r);
        self
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

        // Already queued or running, or finished and waiting for MCR's approval
        // for this same journalist (a re-sent link must not deliver the story
        // twice; another journalist needs their own slug, as with the
        // completed-job rule below)?
        let active: Option<i64> = tx
            .query_row(
                "SELECT id FROM queue
                 WHERE url_normalized = ?
                   AND (status IN ('PENDING','RUNNING')
                        OR (status = 'AWAITING_APPROVAL' AND journalist = ?))
                 ORDER BY id ASC LIMIT 1",
                params![normalized, job.journalist.to_uppercase()],
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
        self.finish_at(job_id, owner, status, error_code, error_message, file_path, Utc::now())
    }

    /// `finish` with the clock passed in; the time the last stage took is
    /// added to `stage_timings_json` in the same transaction.
    fn finish_at(
        &self,
        job_id: i64,
        owner: &str,
        status: JobStatus,
        error_code: Option<&str>,
        error_message: Option<&str>,
        file_path: Option<&str>,
        at: chrono::DateTime<Utc>,
    ) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = timestamps::format(at);
        let timings = stage_timings_for(&tx, "id = ? AND lease_owner = ?", params![job_id, owner], at)?;
        let delivered = matches!(status, JobStatus::Completed);
        let changed = tx.execute(
            r#"
            UPDATE queue
            SET status = ?,
                stage_timings_json = COALESCE(?, stage_timings_json),
                stage = CASE WHEN ? = 'COMPLETED' THEN 'DONE' ELSE stage END,
                lease_owner = NULL,
                lease_expires_at = NULL,
                error_code = ?,
                error_message = ?,
                file_path = COALESCE(?, file_path),
                progress = CASE WHEN ? = 'COMPLETED' THEN 100.0 ELSE progress END,
                completed_at = CASE WHEN ? = 'AWAITING_APPROVAL' THEN completed_at ELSE ? END,
                delivered_at = CASE WHEN ? THEN ? ELSE delivered_at END,
                updated_at = ?
            WHERE id = ? AND lease_owner = ?
            "#,
            params![
                status.as_str(),
                timings,
                status.as_str(),
                error_code,
                error_message,
                file_path,
                status.as_str(),
                status.as_str(),
                now,
                delivered,
                now,
                now,
                job_id,
                owner
            ],
        )?;
        tx.commit()?;
        if changed == 1 {
            let level = if matches!(status, JobStatus::Completed | JobStatus::AwaitingApproval) { "INFO" } else { "ERROR" };
            self.record_event(
                job_id,
                level,
                None,
                &match error_code {
                    Some(code) => format!("Finished as {} ({code})", status.as_str()),
                    None => format!("Finished as {}", status.as_str()),
                },
            )?;
            if let Some(bus) = &self.events {
                let done = delivered;
                bus.job(job_id, status.as_str(), done.then_some("DONE"), done.then_some(100.0));
            }
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
        self.lease_next(owner, lease_secs, false, None)
    }

    /// Lease exactly job `id`, and only while it is PENDING and ready; any
    /// other job is left alone. For tools that made a job themselves and must
    /// never take someone else's. `false`: nothing leased.
    pub fn lease_job_by_id(&self, id: i64, owner: &str, lease_secs: i64) -> Result<bool> {
        Ok(self.lease_next(owner, lease_secs, false, Some(id))?.is_some())
    }

    /// [`Self::lease_job`] limited to jobs MCR approved a held video for
    /// (P10.5). Delivering such a job downloads and encodes nothing, so the
    /// worker loop may take one when every download slot is busy.
    pub fn lease_approved_held_job(&self, owner: &str, lease_secs: i64) -> Result<Option<Job>> {
        self.lease_next(owner, lease_secs, true, None)
    }

    fn lease_next(&self, owner: &str, lease_secs: i64, approved_held_only: bool, only_id: Option<i64>) -> Result<Option<Job>> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let now = timestamps::now_string();
        let select = if only_id.is_some() {
            r#"
                SELECT id FROM queue
                WHERE status = 'PENDING'
                  AND (not_before IS NULL OR not_before <= ?)
                  AND id = ?
                "#
        } else if approved_held_only {
            r#"
                SELECT id FROM queue
                WHERE status = 'PENDING'
                  AND (not_before IS NULL OR not_before <= ?)
                  AND id IN (SELECT job_id FROM held_outputs WHERE decision = 'approved')
                ORDER BY priority DESC, created_at ASC, id ASC
                LIMIT 1
                "#
        } else {
            r#"
                SELECT id FROM queue
                WHERE status = 'PENDING'
                  AND (not_before IS NULL OR not_before <= ?)
                ORDER BY priority DESC, created_at ASC, id ASC
                LIMIT 1
                "#
        };
        let id: Option<i64> = tx
            .query_row(
                select,
                rusqlite::params_from_iter(std::iter::once(rusqlite::types::Value::Text(now.clone())).chain(only_id.map(rusqlite::types::Value::Integer))),
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

    /// 1-based place in the queue of every PENDING job `lease_job` could take
    /// now, in its order. Jobs in back-off have no place (plan P7.19).
    pub fn pending_positions(&self) -> Result<std::collections::HashMap<i64, i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id FROM queue
            WHERE status = 'PENDING'
              AND (not_before IS NULL OR not_before <= ?)
            ORDER BY priority DESC, created_at ASC, id ASC
            "#,
        )?;
        let ids = stmt
            .query_map(params![timestamps::now_string()], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids.into_iter().zip(1..).collect())
    }

    /// Waiting and running figures for the `queue` health check (plan P7.19).
    pub fn queue_watch(&self, now: chrono::DateTime<Utc>) -> Result<QueueWatch> {
        let conn = self.pool.get()?;
        let now_s = timestamps::format(now);
        let (waiting, oldest): (i64, Option<String>) = conn.query_row(
            r#"
            SELECT COUNT(*), MIN(updated_at) FROM queue
            WHERE status = 'PENDING' AND (not_before IS NULL OR not_before <= ?)
            "#,
            params![now_s],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let running: i64 = conn.query_row(
            "SELECT COUNT(*) FROM queue WHERE status = 'RUNNING'",
            [],
            |r| r.get(0),
        )?;
        let step: Option<(i64, String, Option<String>)> = conn
            .query_row(
                r#"
                SELECT id, stage, stage_started_at FROM queue
                WHERE status = 'RUNNING' AND stage_started_at IS NOT NULL
                ORDER BY stage_started_at ASC LIMIT 1
                "#,
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let secs = |raw: Option<String>| {
            timestamps::parse_opt(raw).map(|t| (now - t).num_seconds().max(0))
        };
        Ok(QueueWatch {
            waiting,
            running,
            oldest_ready_wait_secs: secs(oldest),
            longest_step: step.and_then(|(id, stage, at)| secs(at).map(|s| (id, stage, s))),
        })
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

    /// Whether `owner` still holds the lease on a RUNNING job. Read-only: a
    /// check before long work, where `set_stage` would write a stage event.
    pub fn owns_lease(&self, job_id: i64, owner: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let held: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM queue WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
                params![job_id, owner],
                |r| r.get(0),
            )
            .optional()?;
        Ok(held.is_some())
    }

    /// Move a leased job to the next stage and reset the stage clock.
    pub fn set_stage(&self, job_id: i64, owner: &str, stage: JobStage) -> Result<bool> {
        self.set_stage_at(job_id, owner, stage, Utc::now())
    }

    /// `set_stage` with the clock passed in. The time the stage being left took
    /// is added to `stage_timings_json` in the same transaction; a refused move
    /// (lease lost) writes nothing.
    pub fn set_stage_at(&self, job_id: i64, owner: &str, stage: JobStage, at: chrono::DateTime<Utc>) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = timestamps::format(at);
        let timings = stage_timings_for(
            &tx,
            "id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![job_id, owner],
            at,
        )?;
        let changed = tx.execute(
            "UPDATE queue SET stage = ?, stage_started_at = ?, updated_at = ?,
                 stage_timings_json = COALESCE(?, stage_timings_json)
             WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![stage.as_str(), now, now, timings, job_id, owner],
        )?;
        if changed == 1 {
            tx.execute(
                "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, ?, 'INFO', ?)",
                params![job_id, now, stage.as_str(), format!("Stage {}", stage.as_str())],
            )?;
        }
        tx.commit()?;
        if changed == 1 {
            if let Some(bus) = &self.events {
                bus.job(job_id, "RUNNING", Some(stage.as_str()), None);
            }
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
            // A waiting job keeps its updated_at: it is the start of its wait (plan P7.19).
            "UPDATE queue SET keyword = ?, slug = ?,
                    updated_at = CASE WHEN status = 'PENDING' THEN updated_at ELSE ? END
             WHERE id = ?
               AND (status IN ('PENDING', 'REQUIRES_REVIEW', 'MANUAL_DOWNLOAD', 'FAILED')
                    OR (status = 'RUNNING' AND stage IN ('QUEUED', 'EXTRACT', 'DOWNLOAD', 'TRANSCODE')))
               AND NOT EXISTS (SELECT 1 FROM held_outputs h
                               WHERE h.job_id = queue.id AND (h.decision IS NULL OR h.decision = 'approved'))",
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

    /// How this job's video was found (`extraction_method`), written by the
    /// worker holding the lease. `false` when the job is no longer its own.
    pub fn set_leased_extraction_method(&self, job_id: i64, owner: &str, method: Option<&str>) -> Result<bool> {
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE queue SET extraction_method = ?, updated_at = ?
             WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![method, timestamps::now_string(), job_id, owner],
        )?;
        Ok(changed == 1)
    }

    /// A run of the repair ladder begins (P10.11a). Returns the attempt's id.
    pub fn start_attempt(&self, job_id: i64, mode: &str, now: chrono::DateTime<Utc>) -> Result<i64> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO repair_attempts (job_id, started_at, mode) VALUES (?, ?, ?)",
            params![job_id, timestamps::format(now), mode],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The run ended: what it concluded, and the verified candidates in rank order.
    pub fn finish_attempt(&self, id: i64, end: &AttemptEnd, now: chrono::DateTime<Utc>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE repair_attempts
             SET finished_at = ?, rung = ?, outcome = ?, recipe_hash = ?, recipe_id = ?, domain = ?,
                 candidates_json = ?, duration_ms = ?, held_url = ?, next_index = ?, expected_secs = ?,
                 llm_calls = ?, tokens_in = ?, tokens_out = ?
             WHERE id = ?",
            params![
                timestamps::format(now),
                end.rung,
                end.outcome,
                end.recipe_hash,
                end.recipe_id,
                end.domain,
                end.candidates_json,
                end.duration_ms,
                end.held_url,
                end.next_index,
                end.expected_secs,
                end.llm_calls,
                end.tokens_in,
                end.tokens_out,
                id
            ],
        )?;
        Ok(())
    }

    /// The job's most recent repair attempt.
    pub fn latest_attempt(&self, job_id: i64) -> Result<Option<RepairAttempt>> {
        self.attempt_where(job_id, "")
    }

    /// The job's most recent attempt that ended with a held video (the one the
    /// approval card describes).
    pub fn latest_held_attempt(&self, job_id: i64) -> Result<Option<RepairAttempt>> {
        self.attempt_where(job_id, "AND outcome = 'held'")
    }

    /// `extra` is fixed text from the two callers above, never a value.
    fn attempt_where(&self, job_id: i64, extra: &str) -> Result<Option<RepairAttempt>> {
        let conn = self.pool.get()?;
        let sql = format!(
            "SELECT id, job_id, started_at, finished_at, mode, rung, outcome, recipe_hash, recipe_id, domain,
                    llm_calls, tokens_in, tokens_out, candidates_json, next_index, duration_ms,
                    held_url, mcr_decision, image_distance, image_frame_secs, image_host, expected_secs
             FROM repair_attempts WHERE job_id = ? {extra} ORDER BY id DESC LIMIT 1"
        );
        let row = conn
            .query_row(&sql, params![job_id], |r| {
                Ok(RepairAttempt {
                    id: r.get(0)?,
                    job_id: r.get(1)?,
                    started_at: timestamps::parse(&r.get::<_, String>(2)?),
                    finished_at: timestamps::parse_opt(r.get(3)?),
                    mode: r.get(4)?,
                    rung: r.get(5)?,
                    outcome: r.get(6)?,
                    recipe_hash: r.get(7)?,
                    recipe_id: r.get(8)?,
                    domain: r.get(9)?,
                    llm_calls: r.get(10)?,
                    tokens_in: r.get(11)?,
                    tokens_out: r.get(12)?,
                    candidates_json: r.get(13)?,
                    next_index: r.get(14)?,
                    duration_ms: r.get(15)?,
                    held_url: r.get(16)?,
                    mcr_decision: r.get(17)?,
                    image_distance: r.get(18)?,
                    image_frame_secs: r.get(19)?,
                    image_host: r.get(20)?,
                    expected_secs: r.get(21)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// The article-picture evidence of a held attempt (P10.11b), written only
    /// while `owner` still holds the job's lease. `false` when it does not
    /// (nothing is written then).
    pub fn record_card_evidence(
        &self,
        attempt_id: i64,
        job_id: i64,
        owner: &str,
        distance: Option<i64>,
        frame_secs: Option<f64>,
        host: Option<&str>,
    ) -> Result<bool> {
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE repair_attempts SET image_distance = ?, image_frame_secs = ?, image_host = ?
             WHERE id = ? AND job_id = ?
               AND EXISTS (SELECT 1 FROM queue WHERE id = ? AND lease_owner = ? AND status = 'RUNNING')",
            params![distance, frame_secs, host, attempt_id, job_id, job_id, owner],
        )?;
        Ok(changed == 1)
    }

    /// How many times MCR approved this recipe's video on `domain` (0 when it has no row there).
    pub fn recipe_site_approvals(&self, recipe_id: i64, domain: &str) -> Result<i64> {
        let conn = self.pool.get()?;
        let n: Option<i64> = conn
            .query_row(
                "SELECT approvals FROM recipe_sites WHERE recipe_id = ? AND domain = ?",
                params![recipe_id, domain],
                |r| r.get(0),
            )
            .optional()?;
        Ok(n.unwrap_or(0))
    }

    /// The recipe's own full hash, or `None` when it is gone.
    pub fn recipe_hash_of(&self, recipe_id: i64) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row("SELECT hash FROM recipes WHERE id = ?", params![recipe_id], |r| r.get(0))
            .optional()?)
    }

    /// `shipped` | `admin` | `llm`, or `None` when the recipe is gone.
    pub fn recipe_origin(&self, recipe_id: i64) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row("SELECT origin FROM recipes WHERE id = ?", params![recipe_id], |r| r.get(0))
            .optional()?)
    }

    /// Every address MCR rejected for this job: the `held_url` of each of its
    /// attempts with `mcr_decision = 'rejected'`. An expired or approved hold
    /// is not in it.
    pub fn rejected_held_urls(&self, job_id: i64) -> Result<Vec<String>> {
        let conn = self.pool.get()?;
        rejected_urls_in(&conn, job_id)
    }

    /// Reserve one model call for local day `day` (`YYYY-MM-DD`) under `cap`
    /// calls per day, over all jobs. `false`: the cap is reached (nothing is
    /// counted). One immediate transaction, so concurrent jobs cannot both take
    /// the last call; the count is in the database, so a restart keeps it.
    pub fn reserve_llm_call(&self, day: &str, cap: u32) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT OR IGNORE INTO llm_usage (day, calls, tokens_in, tokens_out) VALUES (?, 0, 0, 0)", params![day])?;
        let calls: i64 = tx.query_row("SELECT calls FROM llm_usage WHERE day = ?", params![day], |r| r.get(0))?;
        if calls >= i64::from(cap) {
            return Ok(false);
        }
        tx.execute("UPDATE llm_usage SET calls = calls + 1 WHERE day = ?", params![day])?;
        tx.commit()?;
        Ok(true)
    }

    /// Add the tokens a model call used to the day's total.
    pub fn add_llm_tokens(&self, day: &str, tokens_in: u32, tokens_out: u32) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT OR IGNORE INTO llm_usage (day, calls, tokens_in, tokens_out) VALUES (?, 0, 0, 0)", params![day])?;
        tx.execute(
            "UPDATE llm_usage SET tokens_in = tokens_in + ?, tokens_out = tokens_out + ? WHERE day = ?",
            params![i64::from(tokens_in), i64::from(tokens_out), day],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The calls and tokens counted for `day`: (calls, tokens_in, tokens_out).
    pub fn llm_usage_for(&self, day: &str) -> Result<(i64, i64, i64)> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row("SELECT calls, tokens_in, tokens_out FROM llm_usage WHERE day = ?", params![day], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?
            .unwrap_or((0, 0, 0)))
    }

    /// Was a page bundle with this hash sent to the model at or after `since`?
    pub fn llm_bundle_sent_since(&self, hash: &str, since: chrono::DateTime<Utc>) -> Result<bool> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row(
                "SELECT 1 FROM repair_llm_sent WHERE hash = ? AND at >= ?",
                params![hash, timestamps::format(since)],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Remember that a bundle was sent (a later send of the same hash replaces the time).
    pub fn record_llm_bundle(&self, hash: &str, domain: &str, now: chrono::DateTime<Utc>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO repair_llm_sent (hash, domain, at) VALUES (?, ?, ?)
             ON CONFLICT(hash) DO UPDATE SET domain = excluded.domain, at = excluded.at",
            params![hash, domain, timestamps::format(now)],
        )?;
        Ok(())
    }

    /// Repair attempts on `domain` since `since` that called the model and
    /// found nothing (not held, not shadow-found, not lost with the lease).
    pub fn llm_misses_since(&self, domain: &str, since: chrono::DateTime<Utc>) -> Result<u32> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM repair_attempts
             WHERE domain = ? AND llm_calls > 0 AND started_at >= ?
               AND COALESCE(outcome, '') NOT IN ('held', 'shadow_found', 'lost')",
            params![domain, timestamps::format(since)],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u32)
    }

    /// Stop asking the model about `domain` until `until`.
    pub fn open_llm_breaker(&self, domain: &str, until: chrono::DateTime<Utc>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO site_stats (domain, llm_breaker_until, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(domain) DO UPDATE SET llm_breaker_until = excluded.llm_breaker_until, updated_at = excluded.updated_at",
            params![domain.trim().to_lowercase(), timestamps::format(until), timestamps::now_string()],
        )?;
        Ok(())
    }

    /// Is the model rung shut for `domain` at `now`?
    pub fn llm_breaker_open(&self, domain: &str, now: chrono::DateTime<Utc>) -> Result<bool> {
        let conn = self.pool.get()?;
        let until: Option<Option<String>> = conn
            .query_row(
                "SELECT llm_breaker_until FROM site_stats WHERE domain = ?",
                params![domain.trim().to_lowercase()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(until.flatten().and_then(|t| timestamps::parse_opt(Some(t))).is_some_and(|t| t > now))
    }

    pub fn close_llm_breaker(&self, domain: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE site_stats SET llm_breaker_until = NULL WHERE domain = ?",
            params![domain.trim().to_lowercase()],
        )?;
        Ok(())
    }

    /// Every site's model breaker closes (a new yt-dlp build is kept).
    pub fn close_all_llm_breakers(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        Ok(conn.execute("UPDATE site_stats SET llm_breaker_until = NULL WHERE llm_breaker_until IS NOT NULL", [])?)
    }

    /// Retention: bundles sent before `sent_before` and day totals before day
    /// `usage_before` (`YYYY-MM-DD`).
    pub fn purge_llm_limits(&self, sent_before: chrono::DateTime<Utc>, usage_before: &str) -> Result<usize> {
        let conn = self.pool.get()?;
        let a = conn.execute("DELETE FROM repair_llm_sent WHERE at < ?", params![timestamps::format(sent_before)])?;
        let b = conn.execute("DELETE FROM llm_usage WHERE day < ?", params![usage_before])?;
        Ok(a + b)
    }

    /// Retention: attempts that started more than `days` days ago (their jobs'
    /// own deletion removes the rest).
    pub fn purge_repair_attempts(&self, days: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let cutoff = timestamps::format(Utc::now() - Duration::days(days));
        Ok(conn.execute("DELETE FROM repair_attempts WHERE started_at < ?", params![cutoff])?)
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
            // Already in the watchfolder: finishing it is the only safe
            // recovery, a requeue would put a second copy on air.
            if Self::recover_if_delivered(&tx, id, &now)? {
                continue;
            }
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
        let mut conn = self.pool.get()?;
        let now = timestamps::now_string();
        let prefix = format!("{hostname}:%");

        let ids: Vec<i64> = {
            let mut stmt =
                conn.prepare("SELECT id FROM queue WHERE status = 'RUNNING' AND lease_owner LIKE ?")?;
            let rows = stmt.query_map(params![prefix], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut requeued = 0usize;
        for id in &ids {
            // Each job's change and its event land together or not at all.
            let tx = conn.transaction()?;
            if Self::recover_if_delivered(&tx, *id, &now)? {
                tx.commit()?;
                continue;
            }
            requeued += 1;
            tx.execute(
                "UPDATE queue SET status='PENDING', stage='QUEUED', lease_owner=NULL,
                 lease_expires_at=NULL, progress=0.0, speed='0 Mbps', eta='--:--', updated_at=?
                 WHERE id=?",
                params![now, id],
            )?;
            tx.execute(
                "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, 'QUEUED', 'WARN', ?)",
                params![id, now, "Recovered after daemon restart; requeued"],
            )?;
            tx.commit()?;
        }
        if !ids.is_empty() {
            self.log_audit(
                "WARN",
                "QUEUE",
                &format!("Recovered {} orphaned job(s) after restart ({requeued} requeued)", ids.len()),
            )?;
        }
        Ok(requeued)
    }

    /// A RUNNING job whose video is provably in the watchfolder is finished as
    /// COMPLETED instead of requeued (plan P1.7): a requeue would deliver it a
    /// second time as `_1`. Provable means stage ARCHIVE alone: a lease always
    /// resets the stage to EXTRACT, and the engine enters ARCHIVE only after the
    /// watchfolder rename and the recorded file, so ARCHIVE can only mean this
    /// run delivered. A DELIVER row is not recovered even with a `file_path`
    /// (it may be stale from an earlier attempt, and losing a video is worse
    /// than a duplicate); it requeues as ever. A job whose held output is
    /// approved keeps its own idempotent re-delivery. `delivered_at` is left
    /// as it is: the moment of delivery is not known. The update and its event
    /// are one transaction when `conn` is one.
    fn recover_if_delivered(conn: &rusqlite::Connection, id: i64, now: &str) -> Result<bool> {
        let changed = conn.execute(
            "UPDATE queue SET status='COMPLETED', stage='DONE', lease_owner=NULL,
             lease_expires_at=NULL, progress=100.0, completed_at=?, error_code=NULL, error_message=NULL, updated_at=?
             WHERE id=? AND status='RUNNING' AND stage = 'ARCHIVE'
               AND NOT EXISTS (SELECT 1 FROM held_outputs h WHERE h.job_id = queue.id AND h.decision = 'approved')",
            params![now, now, id],
        )?;
        if changed == 1 {
            conn.execute(
                "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, 'DONE', 'WARN', ?)",
                params![id, now, "RECOVERED_AFTER_DELIVERY: the video was already in the watchfolder; finished, not requeued"],
            )?;
        }
        Ok(changed == 1)
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

    /// Record the delivered file and its duration on a job, without touching
    /// its status: the worker holding the lease decides the terminal status
    /// (`finish`). Only the lease holder may write it.
    pub fn set_delivered_file(&self, id: i64, owner: &str, file_path: &str, duration: f64) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE queue SET file_path = ?, duration_secs = ?, updated_at = ?
             WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![file_path, duration, timestamps::now_string(), id, owner],
        )?;
        Ok(n == 1)
    }

    pub fn update_job_progress(&self, id: i64, progress: f64, speed: &str, eta: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE queue SET progress = ?, speed = ?, eta = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
            params![progress, speed, eta, id],
        )?;
        if let Some(bus) = &self.events {
            // The status and stage are looked up only when the throttle lets an event through.
            bus.job_progress(id, progress, speed, eta, || {
                let row = conn
                    .query_row("SELECT status, stage FROM queue WHERE id = ?", params![id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                    })
                    .optional();
                row.ok().flatten()
            });
        }
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
                "AWAITING_APPROVAL" => summary.approval += count,
                "COMPLETED" | "COMPLETED_MANUAL" => summary.completed += count,
                "FAILED" => summary.failed += count,
                "RUNNING" => summary.running += count,
                "CANCELLED" => summary.cancelled += count,
                // An unknown status is only part of the total: it must not
                // pose as work in progress.
                _ => {}
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
                if filter.include_hidden {
                    "status IN ('REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED')"
                } else {
                    "status IN ('REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED') AND cleared_at IS NULL"
                },
                "updated_at DESC, id DESC",
            ),
            JobsView::Approval => ("status = 'AWAITING_APPROVAL'", "created_at ASC, id ASC"),
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
        // "Today" is the station's day, not UTC's: local midnight, stored as UTC.
        self.job_counts_since(local_midnight_utc())
    }

    /// `job_counts` with the start of "today" given (UTC), so a test can pin it.
    pub fn job_counts_since(&self, day_start_utc: chrono::DateTime<Utc>) -> Result<crate::models::JobCounts> {
        let today_start = timestamps::format(day_start_utc);
        let conn = self.pool.get()?;
        Ok(conn.query_row(
            r#"
            SELECT
              COALESCE(SUM(status IN ('PENDING','RUNNING')), 0),
              COALESCE(SUM(status IN ('COMPLETED','COMPLETED_MANUAL') AND cleared_at IS NULL), 0),
              COALESCE(SUM(status IN ('REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED') AND cleared_at IS NULL), 0),
              COALESCE(SUM(status IN ('COMPLETED','COMPLETED_MANUAL')), 0),
              COALESCE(SUM(status IN ('REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED') AND cleared_at IS NOT NULL), 0),
              COALESCE(SUM(status IN ('COMPLETED','COMPLETED_MANUAL') AND completed_at >= ?), 0),
              COALESCE(SUM(status = 'AWAITING_APPROVAL'), 0)
            FROM queue
            "#,
            params![today_start],
            |r| {
                Ok(crate::models::JobCounts {
                    active: r.get(0)?,
                    finished: r.get(1)?,
                    review: r.get(2)?,
                    completed: r.get(3)?,
                    review_hidden: r.get(4)?,
                    completed_today: r.get(5)?,
                    approval: r.get(6)?,
                })
            },
        )?)
    }

    /// Tidy the review tab (plan P7.21): review-status jobs untouched for
    /// `older_than_hours` leave the list and its badge. Nothing is deleted;
    /// «Εμφάνιση κρυμμένων» brings them back. Returns how many were hidden.
    pub fn hide_old_review(&self, older_than_hours: i64) -> Result<usize> {
        let cutoff = timestamps::format(Utc::now() - Duration::hours(older_than_hours.max(0)));
        let conn = self.pool.get()?;
        Ok(conn.execute(
            "UPDATE queue SET cleared_at = ?
             WHERE status IN ('REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED')
               AND cleared_at IS NULL AND updated_at < ?",
            params![timestamps::now_string(), cutoff],
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

    /// MCR's «Δοκιμή ξανά» / «Χρήση αυτού του συνδέσμου»: the job starts a
    /// clean new run, like a fresh queue entry (attempts, error, back-off and
    /// lease all reset). `cleared_at` is reset too (P7.21): a job tidied away
    /// from the review tab that comes back to life behaves normally. Refused (`false`) for a running or delivered job:
    /// a worker still holds the lease of the first, and the second has its
    /// own action, `redownload_job`.
    pub fn retry_job(&self, id: i64, new_url: Option<&str>) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let n = tx.execute(
            r#"
            UPDATE queue
            SET status = 'PENDING', stage = 'QUEUED', progress = 0.0, speed = '0 Mbps', eta = '--:--',
                attempts = 0, error_message = NULL, error_code = NULL, not_before = NULL,
                lease_owner = NULL, lease_expires_at = NULL, stage_started_at = NULL,
                completed_at = NULL, cleared_at = NULL, url = COALESCE(?, url),
                url_normalized = COALESCE(?, url_normalized), updated_at = ?
            WHERE id = ? AND status IN ('PENDING','REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED','CANCELLED','AWAITING_APPROVAL')
            "#,
            params![new_url, new_url.map(crate::urlnorm::normalize), timestamps::now_string(), id],
        )?;
        if n > 0 {
            // A new run, possibly of another link: a file held (or approved
            // but not yet delivered) for the old run must not be delivered
            // for it (P10.5).
            tx.execute(
                "UPDATE held_outputs SET decision = 'superseded', decided_at = ?
                 WHERE job_id = ? AND (decision IS NULL OR decision = 'approved')",
                params![timestamps::now_string(), id],
            )?;
            // The repair attempts that made those holds are settled too, so a
            // later MCR decision can only land on the new run's attempt (P10.11a).
            tx.execute(
                "UPDATE repair_attempts SET mcr_decision = 'superseded'
                 WHERE job_id = ? AND outcome = 'held' AND mcr_decision IS NULL",
                params![id],
            )?;
            // A «next candidate» run that has not started is dropped: the retry
            // is a clean new run, not the old pick (P10.13).
            tx.execute(
                "UPDATE repair_attempts SET outcome = 'superseded', finished_at = ?
                 WHERE job_id = ? AND rung = 'next' AND outcome IS NULL",
                params![timestamps::now_string(), id],
            )?;
        }
        tx.commit()?;
        if n > 0 {
            self.log_audit("INFO", "QUEUE", &format!("Retried Job #{}", id))?;
        }
        Ok(n > 0)
    }

    /// MCR put the video into Dalet by hand (plan P7.16). Only a job that is
    /// waiting for a person can be marked; `false` otherwise. The error code
    /// and message stay as the history of why it was done by hand, and
    /// `file_path` stays empty: no file was made here.
    pub fn mark_completed_manually(&self, id: i64) -> Result<bool> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let n = conn.execute(
            r#"
            UPDATE queue
            SET status = 'COMPLETED_MANUAL', stage = 'DONE', progress = 100.0, speed = '0 Mbps', eta = '--:--',
                lease_owner = NULL, lease_expires_at = NULL, not_before = NULL,
                completed_at = ?, delivered_at = ?, updated_at = ?, cleared_at = NULL
            WHERE id = ? AND status IN ('REQUIRES_REVIEW','MANUAL_DOWNLOAD','FAILED','AWAITING_APPROVAL')
            "#,
            params![now, now, now, id],
        )?;
        Ok(n > 0)
    }

    /// MCR cancels a video that waits or downloads (plan P7.20). The row and
    /// its history stay as CANCELLED; the lease is taken away, so a worker
    /// still running the job stops before its next stage (P1.14). Once the
    /// job is in DELIVER the file is on its way to the watchfolder and the
    /// cancel is refused.
    pub fn cancel_job(&self, id: i64) -> Result<CancelOutcome> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let n = conn.execute(
            r#"
            UPDATE queue
            SET status = 'CANCELLED', lease_owner = NULL, lease_expires_at = NULL, not_before = NULL,
                completed_at = ?, updated_at = ?
            WHERE id = ? AND (status IN ('PENDING','AWAITING_APPROVAL') OR (status = 'RUNNING' AND stage NOT IN ('DELIVER','ARCHIVE','DONE')))
            "#,
            params![now, now, id],
        )?;
        if n > 0 {
            self.log_audit("WARN", "QUEUE", &format!("Cancelled Job #{}", id))?;
            return Ok(CancelOutcome::Cancelled);
        }
        let row: Option<(String, String)> = conn
            .query_row("SELECT status, stage FROM queue WHERE id = ?", params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(match row {
            Some((status, stage)) if status == "RUNNING" && matches!(stage.as_str(), "DELIVER" | "ARCHIVE" | "DONE") => CancelOutcome::TooLate,
            _ => CancelOutcome::NotCancellable,
        })
    }

    /// A journalist cancels one of their own videos while it still waits
    /// (P7.3). One guarded UPDATE: ownership and PENDING are checked in the
    /// same statement that changes the row, so nothing can slip in between.
    pub fn cancel_own_pending(&self, id: i64, user_id: i64) -> Result<OwnCancelOutcome> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let n = conn.execute(
            r#"
            UPDATE queue
            SET status = 'CANCELLED', lease_owner = NULL, lease_expires_at = NULL, not_before = NULL,
                completed_at = ?, updated_at = ?
            WHERE id = ? AND submitted_by_user_id = ? AND status = 'PENDING'
            "#,
            params![now, now, id, user_id],
        )?;
        if n > 0 {
            return Ok(OwnCancelOutcome::Cancelled);
        }
        let owned: Option<i64> = conn
            .query_row(
                "SELECT id FROM queue WHERE id = ? AND submitted_by_user_id = ?",
                params![id, user_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(if owned.is_some() { OwnCancelOutcome::NotCancellable } else { OwnCancelOutcome::NotFound })
    }

    /// The account's "notify me on completion" choice; `None` for no such user.
    pub fn get_notify_on_completion(&self, user_id: i64) -> Result<Option<bool>> {
        let conn = self.pool.get()?;
        let v: Option<i64> = conn
            .query_row(
                "SELECT notify_on_completion FROM users WHERE id = ?",
                params![user_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.map(|n| n != 0))
    }

    /// Store the choice for this account only; `false` when there is no such user.
    pub fn set_notify_on_completion(&self, user_id: i64, on: bool) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE users SET notify_on_completion = ? WHERE id = ?",
            params![on as i64, user_id],
        )?;
        Ok(n > 0)
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
            "DELETE FROM queue WHERE status IN ('COMPLETED','COMPLETED_MANUAL') AND updated_at < ?",
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
            cleared_at: timestamps::parse_opt(row.get("cleared_at").ok().flatten()),

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

    /// Create the first-run administrator, `Admin` / `Admin` (owner's
    /// decision, 2026-10-08: a fresh installation holds nothing yet, and the
    /// person installing it replaces the account from the panel).
    ///
    /// Only on a database that has never had a user *or* an audit entry: a
    /// brand-new installation, once. An installation whose users were all
    /// removed has an audit trail of it, so the account never comes back on
    /// its own. Returns whether it was created.
    pub fn ensure_default_admin(&self) -> Result<bool> {
        let (users, audit): (i64, i64) = {
            let conn = self.pool.get()?;
            let users = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
            let audit = conn.query_row("SELECT COUNT(*) FROM audit_logs", [], |r| r.get(0))?;
            (users, audit)
        };
        if users > 0 || audit > 0 {
            return Ok(false);
        }
        self.create_user(DEFAULT_ADMIN_LOGIN, DEFAULT_ADMIN_PASSWORD, UserRole::Admin, "First-run administrator", None)?;
        self.log_audit(
            "WARN",
            "AUTH",
            "First-run administrator Admin / Admin created for a new installation: create your own administrator and deactivate it",
        )?;
        Ok(true)
    }

    /// Whether the first-run `Admin` / `Admin` account still logs in.
    pub fn default_admin_still_works(&self) -> bool {
        matches!(
            self.get_user_by_email(DEFAULT_ADMIN_LOGIN),
            Ok(Some(u)) if u.is_active && verify_password(DEFAULT_ADMIN_PASSWORD, &u.password_hash)
        )
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

    /// Fold one outcome into a site's row (plan P10.3). Read-modify-write in
    /// one immediate transaction so two workers finishing at once both count.
    /// Returns the state before and after.
    pub fn record_site_outcome(
        &self,
        domain: &str,
        outcome: SiteOutcome,
    ) -> Result<(SiteState, SiteState)> {
        let domain = domain.trim().to_lowercase();
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row = read_site(&tx, &domain)?.unwrap_or_else(|| SiteRow::new(&domain));
        let before = row.state;
        let now = Utc::now();
        let row = site_health::apply(row, outcome, now);
        let after = row.state;
        tx.execute(
            r#"
            INSERT INTO site_stats (domain, recent, consecutive_fail, last_ok_at, last_fail_at,
                                    last_code, state, state_since, llm_breaker_until, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(domain) DO UPDATE SET
                recent = excluded.recent,
                consecutive_fail = excluded.consecutive_fail,
                last_ok_at = excluded.last_ok_at,
                last_fail_at = excluded.last_fail_at,
                last_code = excluded.last_code,
                state = excluded.state,
                state_since = excluded.state_since,
                llm_breaker_until = excluded.llm_breaker_until,
                updated_at = excluded.updated_at
            "#,
            params![
                row.domain,
                row.recent,
                row.consecutive_fail,
                row.last_ok_at.map(timestamps::format),
                row.last_fail_at.map(timestamps::format),
                row.last_code,
                row.state.as_str(),
                row.state_since.map(timestamps::format),
                row.llm_breaker_until.map(timestamps::format),
                timestamps::format(now),
            ],
        )?;
        tx.commit()?;
        Ok((before, after))
    }

    pub fn get_site(&self, domain: &str) -> Result<Option<SiteRow>> {
        let conn = self.pool.get()?;
        read_site(&conn, &domain.trim().to_lowercase())
    }

    /// Sites that are degraded or broken, oldest trouble first.
    pub fn list_sites_not_healthy(&self) -> Result<Vec<SiteRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT domain, recent, consecutive_fail, last_ok_at, last_fail_at, last_code,
                    state, state_since, llm_breaker_until
             FROM site_stats WHERE state <> 'healthy' ORDER BY state_since ASC, domain ASC",
        )?;
        let rows = stmt
            .query_map([], site_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Set a site's state from outside the outcome stream (canaries).
    /// Returns false when the site has no row.
    pub fn set_site_state(&self, domain: &str, state: SiteState) -> Result<bool> {
        let conn = self.pool.get()?;
        let now = timestamps::now_string();
        let n = conn.execute(
            "UPDATE site_stats
             SET state = ?, state_since = CASE WHEN state <> ? THEN ? ELSE state_since END, updated_at = ?
             WHERE domain = ?",
            params![state.as_str(), state.as_str(), now, now, domain.trim().to_lowercase()],
        )?;
        Ok(n == 1)
    }

    /// Delete rows whose last success and last failure are both older than
    /// `days` (or never happened). Retention.
    pub fn purge_idle_sites(&self, days: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let cutoff = timestamps::format(Utc::now() - Duration::days(days));
        let n = conn.execute(
            "DELETE FROM site_stats
             WHERE (last_ok_at IS NULL OR last_ok_at < ?) AND (last_fail_at IS NULL OR last_fail_at < ?)
               AND (llm_breaker_until IS NULL OR llm_breaker_until < ?)",
            params![cutoff, cutoff, timestamps::now_string()],
        )?;
        Ok(n)
    }

    /// Note that an article of `domain` showed these videos (P3.16). `identities` are the hashes of
    /// every address the article is known by, the address asked for first. They are aliases of one
    /// article: the one an alias already belongs to (a short link first, the direct link later, or the
    /// other way round), else a new one named after the first. When the aliases belong to two
    /// articles stored so far, these are one article and are merged. Hashes only. Seeing the same
    /// video on the same article again moves its `seen_at`, so it is one sighting.
    pub fn record_site_videos(
        &self,
        domain: &str,
        identities: &[String],
        video_hashes: &[String],
        at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let Some(first) = identities.first() else { return Ok(()) };
        if video_hashes.is_empty() {
            return Ok(());
        }
        let domain = domain.trim().to_lowercase();
        let seen = timestamps::format(at);
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut known: Vec<String> = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT article_id FROM site_video_pages WHERE page_hash = ?")?;
            for id in identities {
                if let Some(a) = stmt.query_row(params![id], |r| r.get::<_, String>(0)).optional()? {
                    if !known.contains(&a) {
                        known.push(a);
                    }
                }
            }
        }
        let article = known.first().cloned().unwrap_or_else(|| first.clone());
        for other in known.iter().skip(1) {
            // The other article's sightings move to the survivor; where both saw a video the newer sighting stays.
            tx.execute(
                "INSERT INTO site_videos (domain, video_hash, article_id, seen_at)
                 SELECT domain, video_hash, ?, seen_at FROM site_videos WHERE article_id = ? 
                 ON CONFLICT(domain, video_hash, article_id) DO UPDATE SET seen_at = MAX(seen_at, excluded.seen_at)",
                params![article, other],
            )?;
            tx.execute("DELETE FROM site_videos WHERE article_id = ?", params![other])?;
            tx.execute("UPDATE site_video_pages SET article_id = ? WHERE article_id = ?", params![article, other])?;
        }
        {
            let mut alias = tx.prepare(
                "INSERT INTO site_video_pages (domain, page_hash, article_id, seen_at) VALUES (?, ?, ?, ?)
                 ON CONFLICT(page_hash) DO UPDATE SET seen_at = excluded.seen_at",
            )?;
            for id in identities {
                alias.execute(params![domain, id, article, seen])?;
            }
            let mut sight = tx.prepare(
                "INSERT INTO site_videos (domain, video_hash, article_id, seen_at) VALUES (?, ?, ?, ?)
                 ON CONFLICT(domain, video_hash, article_id) DO UPDATE SET seen_at = excluded.seen_at",
            )?;
            for v in video_hashes {
                sight.execute(params![domain, v, article, seen])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Of `video_hashes`, those seen on at least `SITE_WIDE_MIN_ARTICLES`
    /// different articles of `domain` since `since`, with that number of
    /// articles. `this_article` is every identity of the article being asked about
    /// (see `PageKey::identities`): the articles those addresses belong to are not
    /// other articles, and the article itself counts as one, stored or not. Empty:
    /// only the stored articles count.
    pub fn site_wide(
        &self,
        domain: &str,
        video_hashes: &[String],
        since: chrono::DateTime<Utc>,
        this_article: &[String],
    ) -> Result<std::collections::HashMap<String, usize>> {
        let mut out = std::collections::HashMap::new();
        if video_hashes.is_empty() {
            return Ok(out);
        }
        let domain = domain.trim().to_lowercase();
        let since = timestamps::format(since);
        let conn = self.pool.get()?;
        let mut mine: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT article_id FROM site_video_pages WHERE page_hash = ?")?;
            for id in this_article {
                if let Some(a) = stmt.query_row(params![id], |r| r.get::<_, String>(0)).optional()? {
                    mine.insert(a);
                }
            }
        }
        let mut stmt = conn
            .prepare("SELECT article_id FROM site_videos WHERE domain = ? AND video_hash = ? AND seen_at >= ?")?;
        for v in video_hashes {
            let mut pages: std::collections::HashSet<String> = stmt
                .query_map(params![domain, v, since], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut n = {
                pages.retain(|a| !mine.contains(a));
                pages.len()
            };
            if !this_article.is_empty() {
                n += 1;
            }
            if n >= crate::site_videos::SITE_WIDE_MIN_ARTICLES {
                out.insert(v.clone(), n);
            }
        }
        Ok(out)
    }

    /// Delete sightings last seen before `before`, and the aliases of articles with no sighting
    /// left. Returns the sightings removed. Retention.
    pub fn purge_site_videos(&self, before: chrono::DateTime<Utc>) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute("DELETE FROM site_videos WHERE seen_at < ?", params![timestamps::format(before)])?;
        conn.execute(
            "DELETE FROM site_video_pages WHERE article_id NOT IN (SELECT DISTINCT article_id FROM site_videos)",
            [],
        )?;
        Ok(n)
    }

    /// Clear a site's run of failures after a canary passed (plan P10.4).
    pub fn reset_site_failures(&self, domain: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE site_stats SET consecutive_fail = 0, llm_breaker_until = NULL, updated_at = ? WHERE domain = ?",
            params![timestamps::now_string(), domain.trim().to_lowercase()],
        )?;
        Ok(())
    }

    /// Page links of jobs that completed on their own in the last `days`:
    /// the pool the canaries are drawn from (plan P10.4). Not hand-finished
    /// jobs, not an article's sibling videos, not attachments.
    pub fn completed_pages_since(&self, days: i64) -> Result<Vec<crate::models::CompletedPage>> {
        let conn = self.pool.get()?;
        let cutoff = timestamps::format(Utc::now() - Duration::days(days));
        let mut stmt = conn.prepare(
            "SELECT id, url, COALESCE(completed_at, updated_at) FROM queue
             WHERE status = 'COMPLETED' AND parent_job_id IS NULL
               AND (url LIKE 'http://%' OR url LIKE 'https://%')
               AND COALESCE(completed_at, updated_at) >= ?
             ORDER BY COALESCE(completed_at, updated_at) DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (job_id, url, at) = r?;
            if let Some(finished_at) = timestamps::parse(&at) {
                out.push(crate::models::CompletedPage { job_id, url, finished_at });
            }
        }
        Ok(out)
    }

    /// Every canary, newest first.
    pub fn list_canaries(&self) -> Result<Vec<crate::models::SiteCanary>> {
        let conn = self.pool.get()?;
        read_canaries(&conn, None)
    }

    /// The canaries of one site, newest first.
    pub fn list_canaries_for(&self, domain: &str) -> Result<Vec<crate::models::SiteCanary>> {
        let conn = self.pool.get()?;
        read_canaries(&conn, Some(&domain.trim().to_lowercase()))
    }

    /// Domains that have at least one canary.
    pub fn canary_domains(&self) -> Result<Vec<String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT DISTINCT domain FROM site_canaries ORDER BY domain")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Add a canary; a link already in the pool is left as it is.
    pub fn add_canary(
        &self,
        domain: &str,
        url: &str,
        job_id: Option<i64>,
        added_at: chrono::DateTime<Utc>,
    ) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO site_canaries (domain, url, job_id, added_at) VALUES (?, ?, ?, ?)",
            params![domain.trim().to_lowercase(), url, job_id, timestamps::format(added_at)],
        )?;
        Ok(n == 1)
    }

    pub fn delete_canary(&self, id: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM site_canaries WHERE id = ?", params![id])?;
        Ok(())
    }

    /// The canary's page answered 404/410: the article was taken down. The
    /// row stays, marked, so the link is not added again.
    pub fn mark_canary_gone(&self, id: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE site_canaries SET last_checked_at = ?, last_ok = 0, last_detail = ? WHERE id = ?",
            params![timestamps::now_string(), crate::models::CANARY_GONE, id],
        )?;
        Ok(())
    }

    /// Store a canary check. A pass clears the failure count; a failure adds one.
    pub fn record_canary_result(&self, id: i64, ok: bool, detail: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE site_canaries
             SET last_checked_at = ?, last_ok = ?, last_detail = ?,
                 fail_count = CASE WHEN ? THEN 0 ELSE fail_count + 1 END
             WHERE id = ?",
            params![timestamps::now_string(), ok as i64, detail, ok, id],
        )?;
        Ok(())
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
            "UPDATE queue SET source_path = ?, status = 'PENDING', stage = 'QUEUED', updated_at = ?, cleared_at = NULL
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

    // ---- page evidence (P10.1) ----

    /// Note that the evidence file of a failed job was written.
    pub fn record_evidence(&self, job_id: i64, path: &str, bytes: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR REPLACE INTO job_evidence (job_id, path, bytes, created_at) VALUES (?, ?, ?, ?)",
            params![job_id, path, bytes, timestamps::now_string()],
        )?;
        Ok(())
    }

    /// Every evidence row, oldest first.
    pub fn list_evidence(&self) -> Result<Vec<EvidenceRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, path, bytes, created_at FROM job_evidence ORDER BY created_at ASC, job_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(EvidenceRow {
                job_id: r.get(0)?,
                path: r.get(1)?,
                bytes: r.get(2)?,
                created_at: timestamps::parse_opt(r.get(3).ok()),
            })
        })?;
        let mut list = Vec::new();
        for row in rows {
            list.push(row?);
        }
        Ok(list)
    }

    pub fn delete_evidence(&self, job_id: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM job_evidence WHERE job_id = ?", params![job_id])?;
        Ok(())
    }
}

/// What the held-output sweep does with one row (P10.5), decided from the
/// row, its job and the clock alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldSweep {
    Keep,
    /// Past its time, still waiting: the job goes to review.
    Expire,
    /// MCR (or a retry) moved the job on: the held file is no longer wanted.
    Supersede,
    /// The job is gone: the row goes too.
    Forget,
}

/// The sweep's decision for a held row. `job` is `None` when the job no
/// longer exists. A waiting row whose expiry does not parse is expired
/// rather than kept for ever.
pub fn held_sweep_action(
    decision: Option<&str>,
    job: Option<JobStatus>,
    expires_at: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> HeldSweep {
    let Some(job) = job else { return HeldSweep::Forget };
    match decision {
        None => match job {
            JobStatus::AwaitingApproval if expires_at.map_or(true, |t| t < now) => HeldSweep::Expire,
            // Running: the hold is being written right now.
            JobStatus::AwaitingApproval | JobStatus::Running => HeldSweep::Keep,
            _ => HeldSweep::Supersede,
        },
        Some("approved") => match job {
            JobStatus::Pending | JobStatus::Running => HeldSweep::Keep,
            _ => HeldSweep::Supersede,
        },
        Some(_) => HeldSweep::Keep,
    }
}

// ---- videos held for approval (P10.5) ----
impl Repository {
    fn held_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<HeldOutput> {
        Ok(HeldOutput {
            job_id: r.get("job_id")?,
            path: r.get("path")?,
            bytes: r.get("bytes")?,
            sha256: r.get("sha256")?,
            duration: r.get("duration")?,
            created_at: timestamps::parse_opt(r.get("created_at")?),
            expires_at: timestamps::parse_opt(r.get("expires_at")?),
            decision: r.get("decision")?,
            decided_by: r.get("decided_by")?,
            decided_at: timestamps::parse_opt(r.get("decided_at")?),
            reason: r.get("reason")?,
            delivered_path: r.get("delivered_path")?,
        })
    }

    /// Record the held file of a job this worker still leases. `false` when
    /// the lease is gone: nothing is written. Replaces an older row of the
    /// same job (a retried job holds again).
    pub fn hold_output(
        &self,
        job_id: i64,
        owner: &str,
        path: &str,
        bytes: i64,
        sha256: &str,
        duration: Option<f64>,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let ours: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM queue WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
                params![job_id, owner],
                |r| r.get(0),
            )
            .optional()?;
        if ours.is_none() {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR REPLACE INTO held_outputs
               (job_id, path, bytes, sha256, duration, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![job_id, path, bytes, sha256, duration, timestamps::now_string(), timestamps::format(expires_at)],
        )?;
        // Nothing of this run was delivered, whatever an earlier run left:
        // a stale file_path must never pass for a delivery.
        tx.execute(
            "UPDATE queue SET file_path = NULL, delivered_at = NULL WHERE id = ?",
            params![job_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The held row of a job, whatever its decision.
    pub fn get_held(&self, job_id: i64) -> Result<Option<HeldOutput>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row("SELECT * FROM held_outputs WHERE job_id = ?", params![job_id], Self::held_from_row)
            .optional()?)
    }

    /// The held row while nobody has decided on it.
    pub fn waiting_held(&self, job_id: i64) -> Result<Option<HeldOutput>> {
        Ok(self.get_held(job_id)?.filter(|h| h.decision.is_none()))
    }

    /// The held row once MCR approved it and it is not yet delivered.
    pub fn approved_held(&self, job_id: i64) -> Result<Option<HeldOutput>> {
        Ok(self.get_held(job_id)?.filter(|h| h.decision.as_deref() == Some("approved")))
    }

    /// Bytes held for approval or approved-and-not-yet-delivered, other than
    /// job `except`'s own (which a new hold replaces).
    pub fn held_bytes_in_use(&self, except: i64) -> Result<u64> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM held_outputs
             WHERE job_id != ? AND (decision IS NULL OR decision = 'approved')",
            params![except],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// Right after the approved file was copied: the job's `file_path` and
    /// the row's `delivered_path` are written together, owner-checked. A
    /// restart that finds both equal knows this file was delivered and
    /// copies nothing again. `false` when the lease is gone (nothing written).
    pub fn record_held_delivery(&self, job_id: i64, owner: &str, path: &str, duration: f64) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE queue SET file_path = ?, duration_secs = ?, updated_at = ?
             WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![path, duration, timestamps::now_string(), job_id, owner],
        )?;
        if n != 1 {
            return Ok(false);
        }
        tx.execute(
            "UPDATE held_outputs SET delivered_path = ? WHERE job_id = ? AND decision = 'approved'",
            params![path, job_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The end of an approved delivery, in one transaction: the job is
    /// COMPLETED with the file the copy recorded, and the held row is
    /// `delivered`. Refused (`false`, nothing written) when the lease is gone
    /// or the job's `file_path` is not the one this approved file was
    /// delivered to.
    pub fn finish_held_delivery(&self, job_id: i64, owner: &str) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = timestamps::now_string();
        let timings = stage_timings_for(
            &tx,
            "id = ? AND lease_owner = ? AND status = 'RUNNING'",
            params![job_id, owner],
            Utc::now(),
        )?;
        let n = tx.execute(
            r#"
            UPDATE queue
            SET status = 'COMPLETED', stage = 'DONE', lease_owner = NULL, lease_expires_at = NULL,
                error_code = NULL, error_message = NULL, progress = 100.0,
                completed_at = ?, delivered_at = ?, updated_at = ?,
                stage_timings_json = COALESCE(?, stage_timings_json)
            WHERE id = ? AND lease_owner = ? AND status = 'RUNNING'
              AND file_path IS NOT NULL AND file_path != ''
              AND file_path = (SELECT delivered_path FROM held_outputs
                               WHERE job_id = queue.id AND decision = 'approved')
            "#,
            params![now, now, now, timings, job_id, owner],
        )?;
        if n != 1 {
            return Ok(false);
        }
        tx.execute(
            "UPDATE held_outputs SET decision = 'delivered', decided_at = ? WHERE job_id = ? AND decision = 'approved'",
            params![now, job_id],
        )?;
        tx.execute(
            "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, NULL, 'INFO', 'Finished as COMPLETED')",
            params![job_id, now],
        )?;
        tx.commit()?;
        if let Some(bus) = &self.events {
            bus.job(job_id, "COMPLETED", Some("DONE"), Some(100.0));
        }
        Ok(true)
    }

    /// Set the decision of an approved row (`delivered`, once its file is in
    /// the watchfolder). `false` when the row was not approved.
    pub fn mark_held(&self, job_id: i64, decision: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE held_outputs SET decision = ?, decided_at = ?
             WHERE job_id = ? AND decision = 'approved'",
            params![decision, timestamps::now_string(), job_id],
        )?;
        Ok(n == 1)
    }

    /// MCR approves the held file with this SHA-256: the job goes back to
    /// PENDING and the worker that leases it delivers exactly that file.
    /// One transaction; `false` (nothing changed) when the job is not
    /// waiting, the file is not the one named, or it was decided already.
    pub fn approve_held(&self, job_id: i64, sha256: &str, by: &str) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = timestamps::now_string();
        let held = tx.execute(
            "UPDATE held_outputs SET decision = 'approved', decided_by = ?, decided_at = ?
             WHERE job_id = ? AND decision IS NULL AND sha256 = ?",
            params![by, now, job_id, sha256],
        )?;
        let queued = tx.execute(
            r#"
            UPDATE queue
            SET status = 'PENDING', stage = 'QUEUED', progress = 0.0, speed = '0 Mbps', eta = '--:--',
                attempts = 0, error_message = NULL, error_code = NULL, not_before = NULL,
                lease_owner = NULL, lease_expires_at = NULL, stage_started_at = NULL,
                completed_at = NULL, cleared_at = NULL, file_path = NULL, delivered_at = NULL,
                updated_at = ?
            WHERE id = ? AND status = 'AWAITING_APPROVAL'
            "#,
            params![now, job_id],
        )?;
        if held != 1 || queued != 1 {
            return Ok(false);
        }
        note_mcr_decision(&tx, job_id, "approved", None, Learn::Approved, chrono::Utc::now())?;
        tx.execute(
            "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, NULL, 'INFO', ?)",
            params![job_id, now, format!("Approved by {by}; delivering the held file")],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// MCR rejects the held file: the job goes to review with
    /// REJECTED_BY_MCR and `message`. One transaction; `false` when the job
    /// is not waiting.
    pub fn reject_held(&self, job_id: i64, reason: &str, message: &str, by: &str) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = timestamps::now_string();
        let held = tx.execute(
            "UPDATE held_outputs SET decision = 'rejected', decided_by = ?, decided_at = ?, reason = ?
             WHERE job_id = ? AND decision IS NULL",
            params![by, now, reason, job_id],
        )?;
        let queued = tx.execute(
            r#"
            UPDATE queue
            SET status = 'REQUIRES_REVIEW', error_code = 'REJECTED_BY_MCR', error_message = ?,
                lease_owner = NULL, lease_expires_at = NULL, not_before = NULL,
                completed_at = ?, cleared_at = NULL, updated_at = ?
            WHERE id = ? AND status = 'AWAITING_APPROVAL'
            "#,
            params![message, now, now, job_id],
        )?;
        if held != 1 || queued != 1 {
            return Ok(false);
        }
        note_mcr_decision(&tx, job_id, "rejected", Some(reason), if reason == "wrong_video" { Learn::WrongVideo } else { Learn::Nothing }, chrono::Utc::now())?;
        tx.execute(
            "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, NULL, 'WARN', ?)",
            params![job_id, now, format!("Rejected by {by} ({reason}): {message}")],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// A waiting hold past its time: the job goes to review with
    /// APPROVAL_EXPIRED. `false` when it was decided in the meantime.
    pub fn expire_held(&self, job_id: i64, now: chrono::DateTime<Utc>) -> Result<bool> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let at = timestamps::format(now);
        let held = tx.execute(
            "UPDATE held_outputs SET decision = 'expired', decided_at = ?
             WHERE job_id = ? AND decision IS NULL",
            params![at, job_id],
        )?;
        let queued = tx.execute(
            r#"
            UPDATE queue
            SET status = 'REQUIRES_REVIEW', error_code = 'APPROVAL_EXPIRED',
                error_message = 'Nobody approved the video in time; the held file was deleted',
                lease_owner = NULL, lease_expires_at = NULL, not_before = NULL,
                completed_at = ?, cleared_at = NULL, updated_at = ?
            WHERE id = ? AND status = 'AWAITING_APPROVAL'
            "#,
            params![at, at, job_id],
        )?;
        if held != 1 || queued != 1 {
            return Ok(false);
        }
        note_mcr_decision(&tx, job_id, "expired", None, Learn::Nothing, now)?;
        tx.execute(
            "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, NULL, 'WARN', ?)",
            params![job_id, at, "Nobody approved the held video in time; it was deleted"],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// MCR's «Επόμενη επιλογή» (P10.13): the held video is called the wrong
    /// one (everything `reject_held(.., "wrong_video", ..)` does: the decision,
    /// the recipe's rejection, the skip) and the job goes back to PENDING with a
    /// new `next` repair attempt that names the next verified candidate of the
    /// same run. The worker that leases it downloads that one, held again; nothing
    /// is searched again. One transaction. `None` (nothing changed): the job is
    /// not waiting, the file is not the one named, or no candidate is left.
    /// Otherwise the place of the chosen candidate in the attempt's list.
    pub fn next_candidate(&self, job_id: i64, sha256: &str, by: &str, now: chrono::DateTime<Utc>) -> Result<Option<usize>> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let waiting: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM held_outputs h JOIN queue q ON q.id = h.job_id
                 WHERE h.job_id = ? AND h.decision IS NULL AND h.sha256 = ? AND q.status = 'AWAITING_APPROVAL'",
                params![job_id, sha256],
                |r| r.get(0),
            )
            .optional()?;
        if waiting.is_none() {
            return Ok(None);
        }
        let Some(hold) = open_hold(&tx, job_id)? else { return Ok(None) };
        let Some(domain) = hold.domain.clone() else { return Ok(None) };
        let mut excluded = rejected_urls_in(&tx, job_id)?;
        excluded.extend(hold.held_url.clone());
        let skips = skips_of(&tx, &domain, now)?;
        let Some(&index) = next_options(hold.candidates_json.as_deref(), hold.next_index, &excluded, &skips).first() else {
            return Ok(None);
        };
        let at = timestamps::format(now);
        let held = tx.execute(
            "UPDATE held_outputs SET decision = 'rejected', decided_by = ?, decided_at = ?, reason = 'wrong_video'
             WHERE job_id = ? AND decision IS NULL",
            params![by, at, job_id],
        )?;
        // Back to the queue like a retry, but the history of the run stays.
        let queued = tx.execute(
            r#"
            UPDATE queue
            SET status = 'PENDING', stage = 'QUEUED', progress = 0.0, speed = '0 Mbps', eta = '--:--',
                attempts = 0, error_message = NULL, error_code = NULL, not_before = NULL,
                lease_owner = NULL, lease_expires_at = NULL, stage_started_at = NULL,
                completed_at = NULL, cleared_at = NULL, file_path = NULL, delivered_at = NULL,
                updated_at = ?
            WHERE id = ? AND status = 'AWAITING_APPROVAL'
            "#,
            params![at, job_id],
        )?;
        if held != 1 || queued != 1 {
            return Ok(None);
        }
        note_mcr_decision(&tx, job_id, "rejected", Some("wrong_video"), Learn::WrongVideo, now)?;
        tx.execute(
            "INSERT INTO repair_attempts (job_id, started_at, mode, rung, candidates_json, next_index, domain)
             VALUES (?, ?, 'hold', 'next', ?, ?, ?)",
            params![job_id, at, hold.candidates_json, index as i64, domain],
        )?;
        tx.execute(
            "INSERT INTO job_events (job_id, at, stage, level, message) VALUES (?, ?, NULL, 'WARN', ?)",
            params![job_id, at, format!("Rejected by {by} (wrong video); trying the next verified candidate")],
        )?;
        tx.commit()?;
        Ok(Some(index))
    }

    /// How many verified candidates MCR could still ask for after the held one
    /// of this job (the count behind «Επόμενη επιλογή (n)»).
    pub fn next_left(&self, job_id: i64, now: chrono::DateTime<Utc>) -> Result<usize> {
        let Some(hold) = self.latest_held_attempt(job_id)? else { return Ok(0) };
        let Some(domain) = hold.domain.as_deref() else { return Ok(0) };
        let mut excluded = self.rejected_held_urls(job_id)?;
        excluded.extend(hold.held_url.clone());
        let skips = self.repair_skip_patterns(domain, now)?;
        Ok(next_options(hold.candidates_json.as_deref(), hold.next_index, &excluded, &skips).len())
    }

    /// The wrong-video patterns MCR set for `domain` that still hold at `now`.
    pub fn repair_skip_patterns(&self, domain: &str, now: chrono::DateTime<Utc>) -> Result<Vec<String>> {
        let conn = self.pool.get()?;
        skips_of(&conn, domain, now)
    }

    /// Retention: skips that ran out before `now`.
    pub fn purge_repair_skips(&self, now: chrono::DateTime<Utc>) -> Result<usize> {
        let conn = self.pool.get()?;
        Ok(conn.execute("DELETE FROM repair_skips WHERE until <= ?", params![timestamps::format(now)])?)
    }

    /// One overnight self-heal run on a broken site (P10.14), kept 30 days and
    /// at most [`SELFHEAL_MAX_ROWS`] rows (the oldest go first). Hosts and a
    /// recipe hash only: no page address ever lands here.
    pub fn record_selfheal(
        &self,
        domain: &str,
        outcome: &str,
        recipe_hash: Option<&str>,
        origin: Option<&str>,
        pages: usize,
        detail: Option<&str>,
        now: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO selfheal_reports (domain, at, outcome, recipe_hash, origin, pages, detail)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![domain.trim().to_lowercase(), timestamps::format(now), outcome, recipe_hash, origin, pages as i64, detail],
        )?;
        tx.execute(
            "DELETE FROM selfheal_reports WHERE id NOT IN (SELECT id FROM selfheal_reports ORDER BY id DESC LIMIT ?)",
            params![SELFHEAL_MAX_ROWS],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The self-heal runs since `since`, newest first.
    pub fn selfheal_since(&self, since: chrono::DateTime<Utc>) -> Result<Vec<SelfhealReport>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, domain, at, outcome, recipe_hash, origin, pages, detail FROM selfheal_reports
             WHERE at >= ? ORDER BY at DESC, id DESC",
        )?;
        let rows = stmt
            .query_map(params![timestamps::format(since)], |r| {
                Ok(SelfhealReport {
                    id: r.get(0)?,
                    domain: r.get(1)?,
                    at: timestamps::parse(&r.get::<_, String>(2)?),
                    outcome: r.get(3)?,
                    recipe_hash: r.get(4)?,
                    origin: r.get(5)?,
                    pages: r.get(6)?,
                    detail: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Retention: self-heal runs older than 30 days.
    pub fn purge_selfheal(&self, now: chrono::DateTime<Utc>) -> Result<usize> {
        let conn = self.pool.get()?;
        let cutoff = timestamps::format(now - Duration::days(SELFHEAL_KEEP_DAYS));
        Ok(conn.execute("DELETE FROM selfheal_reports WHERE at < ?", params![cutoff])?)
    }

    /// Debug builds only, for `dev-seed-repair`: what MCR decided about an
    /// attempt and the article-picture distance the card measured.
    #[cfg(debug_assertions)]
    pub fn dev_decide_attempt(&self, attempt_id: i64, decision: Option<&str>, image_distance: Option<i64>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE repair_attempts SET mcr_decision = ?, image_distance = ? WHERE id = ?",
            params![decision, image_distance, attempt_id],
        )?;
        Ok(())
    }

    /// Every site the daemon keeps health for, the broken first, with its
    /// canaries (P10.15a). An LLM breaker that has run out is not reported.
    pub fn list_sites(&self, now: chrono::DateTime<Utc>) -> Result<Vec<SiteListRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT s.domain, s.recent, s.consecutive_fail, s.last_ok_at, s.last_fail_at, s.last_code,
                    s.state, s.state_since, s.llm_breaker_until,
                    (SELECT COUNT(*) FROM site_canaries c WHERE c.domain = s.domain
                        AND (COALESCE(c.last_ok, -1) <> 0 OR COALESCE(c.last_detail, '') <> ?1)),
                    (SELECT COUNT(*) FROM site_canaries c WHERE c.domain = s.domain AND c.last_ok = 1),
                    (SELECT MAX(c.last_checked_at) FROM site_canaries c WHERE c.domain = s.domain)
             FROM site_stats s
             ORDER BY CASE s.state WHEN 'broken' THEN 0 WHEN 'degraded' THEN 1 ELSE 2 END, s.domain",
        )?;
        let rows = stmt
            .query_map(params![crate::models::CANARY_GONE], |r| {
                let mut site = site_from_row(r)?;
                site.llm_breaker_until = site.llm_breaker_until.filter(|t| *t > now);
                Ok(SiteListRow {
                    site,
                    canaries: r.get::<_, i64>(9)?.max(0) as usize,
                    canaries_ok: r.get::<_, i64>(10)?.max(0) as usize,
                    canary_checked_at: timestamps::parse_opt(r.get::<_, Option<String>>(11)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// What the repair ladder did per site since `since` (P10.15a). Attempts
    /// with no domain are one row, `(unknown)`. MCR's «next choice» rows are
    /// counted apart from the ladder's own runs. A rejection counts as «wrong
    /// video» by the reason stored with the decision (`mcr_reason`); a row from
    /// before that column has none and counts as another reason.
    pub fn repair_report(&self, since: chrono::DateTime<Utc>) -> Result<Vec<SiteReport>> {
        let conn = self.pool.get()?;
        let since = timestamps::format(since);
        let mut stmt = conn.prepare(
            "SELECT a.domain, a.mode, a.outcome, a.mcr_decision, a.rung, a.expected_secs, a.image_distance,
                    a.llm_calls, a.tokens_in, a.tokens_out, a.duration_ms, a.mcr_reason
             FROM repair_attempts a WHERE a.started_at >= ? ORDER BY a.id",
        )?;
        let rows = stmt
            .query_map(params![since], |r| {
                Ok(ReportRow {
                    domain: r.get(0)?,
                    mode: r.get(1)?,
                    outcome: r.get(2)?,
                    decision: r.get(3)?,
                    rung: r.get(4)?,
                    wrong_video: r.get::<_, Option<String>>(11)?.as_deref() == Some("wrong_video"),
                    expected_secs: r.get(5)?,
                    image_distance: r.get(6)?,
                    llm_calls: r.get(7)?,
                    tokens_in: r.get(8)?,
                    tokens_out: r.get(9)?,
                    duration_ms: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut report = aggregate_report(rows);
        let mut stmt = conn.prepare(
            "SELECT domain, COUNT(*), COALESCE(SUM(outcome = 'found'), 0) FROM selfheal_reports
             WHERE at >= ? AND outcome IN ('found', 'nothing') GROUP BY domain",
        )?;
        let heals = stmt
            .query_map(params![since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (domain, runs, found) in heals {
            match report.iter_mut().find(|s| s.domain == domain) {
                Some(s) => {
                    s.heal_runs = runs;
                    s.heal_found = found;
                }
                None => report.push(SiteReport { domain, heal_runs: runs, heal_found: found, ..Default::default() }),
            }
        }
        report.sort_by(|a, b| b.attempts.cmp(&a.attempts).then_with(|| a.domain.cmp(&b.domain)));
        Ok(report)
    }

    /// A held row whose job moved on without it: the condition of
    /// [`held_sweep_action`], checked again in SQL.
    pub fn supersede_held(&self, job_id: i64) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE held_outputs SET decision = 'superseded', decided_at = ?
             WHERE job_id = ? AND (
               (decision IS NULL AND COALESCE((SELECT status FROM queue WHERE id = job_id), '')
                  NOT IN ('AWAITING_APPROVAL','RUNNING','')) OR
               (decision = 'approved' AND COALESCE((SELECT status FROM queue WHERE id = job_id), '')
                  NOT IN ('PENDING','RUNNING','')))",
            params![timestamps::now_string(), job_id],
        )?;
        Ok(n == 1)
    }

    /// Drop the row of a job that no longer exists.
    pub fn forget_held(&self, job_id: i64) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "DELETE FROM held_outputs WHERE job_id = ? AND NOT EXISTS (SELECT 1 FROM queue WHERE id = ?)",
            params![job_id, job_id],
        )?;
        Ok(n == 1)
    }

    /// Every row that still wants its file (waiting or approved), with its
    /// job's status (`None` when the job is gone).
    pub fn list_live_held(&self) -> Result<Vec<(HeldOutput, Option<JobStatus>)>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT h.*, (SELECT status FROM queue WHERE id = h.job_id) AS job_status
             FROM held_outputs h
             WHERE h.decision IS NULL OR h.decision = 'approved'
             ORDER BY h.job_id",
        )?;
        let rows = stmt.query_map([], |r| {
            let status: Option<String> = r.get("job_status")?;
            Ok((Self::held_from_row(r)?, status.as_deref().and_then(JobStatus::parse)))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

// ---------------------------------------------------------------------------
// Recipe store (plan P10.10). Rules live in `recipe_score`; this is the SQL.
//
// Every public method is one IMMEDIATE transaction. A path that answers
// "over cap" or "refused" drops the transaction, so it leaves no trace.
// ---------------------------------------------------------------------------

mod recipe_sql {
    use super::*;
    use crate::recipe_score::{
        self as rs, Event, InsertOutcome, NewRecipe, Origin, PromoSource, RankKey, RankedRecipe, RecipeCounts,
        RecipeGcReport, RecipeMeta, RecipeState, RetiredReason, ScopeKind, ShippedRecipe, SiteRow, SiteState,
        SuspendReason,
    };
    use chrono::DateTime;
    use rusqlite::Connection;
    use std::collections::HashMap;

    // Column lists as macros so statements are built with `concat!`, never `format!`.
    macro_rules! site_cols {
        () => {
            "alpha, beta, scored_at, created_at, approvals, rejections, uses, consecutive_fail, pass_streak, state, suspended_reason, suspended_at, last_used_at, last_ok_at"
        };
    }
    macro_rules! head_cols {
        () => {
            "id, hash, steps_hash, scope_kind, scope_key, family, origin, state, created_at, retired_at, retired_reason, revived_at"
        };
    }

    fn ts(d: DateTime<Utc>) -> String {
        timestamps::format(d)
    }

    fn ts_opt(d: Option<DateTime<Utc>>) -> Option<String> {
        d.map(timestamps::format)
    }

    fn parse_opt(s: Option<String>) -> Option<DateTime<Utc>> {
        s.as_deref().and_then(timestamps::parse)
    }

    /// An unreadable required time counts as the oldest possible one: the row
    /// decays away and retention removes it, instead of looking fresh.
    fn parse_required(s: String) -> DateTime<Utc> {
        timestamps::parse(&s).unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
    }

    fn site_row_at(r: &rusqlite::Row, o: usize) -> rusqlite::Result<SiteRow> {
        let state: String = r.get(o + 9)?;
        let reason: Option<String> = r.get(o + 10)?;
        Ok(SiteRow {
            alpha: r.get(o)?,
            beta: r.get(o + 1)?,
            scored_at: parse_required(r.get(o + 2)?),
            created_at: parse_required(r.get(o + 3)?),
            approvals: r.get(o + 4)?,
            rejections: r.get(o + 5)?,
            uses: r.get(o + 6)?,
            consecutive_fail: r.get(o + 7)?,
            pass_streak: r.get(o + 8)?,
            state: SiteState::parse(&state).unwrap_or(SiteState::Candidate),
            suspended_reason: reason.as_deref().and_then(SuspendReason::parse),
            suspended_at: parse_opt(r.get(o + 11)?),
            last_used_at: parse_opt(r.get(o + 12)?),
            last_ok_at: parse_opt(r.get(o + 13)?),
        })
    }

    fn load_rows(conn: &Connection, recipe_id: i64) -> Result<Vec<(String, SiteRow)>> {
        let mut stmt = conn.prepare(concat!(
            "SELECT domain, ", site_cols!(), " FROM recipe_sites WHERE recipe_id = ? ORDER BY domain"
        ))?;
        let rows = stmt
            .query_map(params![recipe_id], |r| Ok((r.get::<_, String>(0)?, site_row_at(r, 1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn store_row(conn: &Connection, recipe_id: i64, domain: &str, r: &SiteRow) -> Result<()> {
        conn.execute(
            "INSERT INTO recipe_sites (recipe_id, domain, alpha, beta, scored_at, created_at, approvals, rejections,
                 uses, consecutive_fail, pass_streak, state, suspended_reason, suspended_at, last_used_at, last_ok_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(recipe_id, domain) DO UPDATE SET
                 alpha = excluded.alpha, beta = excluded.beta, scored_at = excluded.scored_at,
                 created_at = excluded.created_at, approvals = excluded.approvals,
                 rejections = excluded.rejections, uses = excluded.uses,
                 consecutive_fail = excluded.consecutive_fail, pass_streak = excluded.pass_streak,
                 state = excluded.state, suspended_reason = excluded.suspended_reason,
                 suspended_at = excluded.suspended_at, last_used_at = excluded.last_used_at,
                 last_ok_at = excluded.last_ok_at",
            params![
                recipe_id,
                domain,
                r.alpha,
                r.beta,
                ts(r.scored_at),
                ts(r.created_at),
                r.approvals,
                r.rejections,
                r.uses,
                r.consecutive_fail,
                r.pass_streak,
                r.state.as_str(),
                r.suspended_reason.map(|x| x.as_str()),
                ts_opt(r.suspended_at),
                ts_opt(r.last_used_at),
                ts_opt(r.last_ok_at),
            ],
        )?;
        Ok(())
    }

    /// A recipe's header row. Unreadable enum text falls back to the least
    /// privileged reading; only this code writes the table.
    struct Head {
        id: i64,
        hash: String,
        steps_hash: String,
        scope_kind: ScopeKind,
        scope_key: String,
        origin: Origin,
        state: RecipeState,
        created_at: DateTime<Utc>,
        revived_at: Option<DateTime<Utc>>,
        retired_reason: Option<RetiredReason>,
    }

    fn head_from_row(r: &rusqlite::Row) -> rusqlite::Result<Head> {
        let kind: String = r.get(3)?;
        let origin: String = r.get(6)?;
        let state: String = r.get(7)?;
        let reason: Option<String> = r.get(10)?;
        Ok(Head {
            id: r.get(0)?,
            hash: r.get(1)?,
            steps_hash: r.get(2)?,
            scope_kind: ScopeKind::parse(&kind).unwrap_or(ScopeKind::Site),
            scope_key: r.get(4)?,
            origin: Origin::parse(&origin).unwrap_or(Origin::Llm),
            state: RecipeState::parse(&state).unwrap_or(RecipeState::Candidate),
            created_at: parse_required(r.get(8)?),
            revived_at: parse_opt(r.get(11)?),
            retired_reason: reason.as_deref().and_then(RetiredReason::parse),
        })
    }

    fn head_by_hash(conn: &Connection, hash: &str) -> Result<Option<Head>> {
        Ok(conn
            .query_row(concat!("SELECT ", head_cols!(), " FROM recipes WHERE hash = ?"), params![hash], head_from_row)
            .optional()?)
    }

    fn head_by_id(conn: &Connection, id: i64) -> Result<Option<Head>> {
        Ok(conn
            .query_row(concat!("SELECT ", head_cols!(), " FROM recipes WHERE id = ?"), params![id], head_from_row)
            .optional()?)
    }

    fn count(conn: &Connection, sql: &str) -> Result<usize> {
        let n: i64 = conn.query_row(sql, [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }

    /// Recompute a non-retired recipe's state from its site rows.
    fn recompute_state(conn: &Connection, id: i64) -> Result<()> {
        let Some(head) = head_by_id(conn, id)? else { return Ok(()) };
        if head.state == RecipeState::Retired {
            return Ok(());
        }
        let states: Vec<SiteState> = load_rows(conn, id)?.into_iter().map(|(_, r)| r.state).collect();
        let next = rs::recipe_state(head.origin, head.state, &states);
        if next != head.state {
            conn.execute("UPDATE recipes SET state = ? WHERE id = ?", params![next.as_str(), id])?;
        }
        Ok(())
    }

    fn retire(conn: &Connection, id: i64, reason: RetiredReason, now: DateTime<Utc>) -> Result<()> {
        conn.execute(
            "UPDATE recipes SET state = 'retired', retired_at = ?, retired_reason = ? WHERE id = ?",
            params![ts(now), reason.as_str(), id],
        )?;
        Ok(())
    }

    fn delete_recipe(conn: &Connection, id: i64) -> Result<()> {
        // Explicit, so a connection without foreign keys cannot leave orphans.
        conn.execute("DELETE FROM recipe_sites WHERE recipe_id = ?", params![id])?;
        conn.execute("DELETE FROM recipes WHERE id = ?", params![id])?;
        Ok(())
    }

    /// Keep at most `limit` tombstones. Those that refuse nothing go first, oldest
    /// first: a recent `rejected` or `admin` tombstone is the one that must last.
    fn trim_tombstones(conn: &Connection, limit: usize) -> Result<usize> {
        let excess = count(conn, "SELECT COUNT(*) FROM recipe_tombstones")?.saturating_sub(limit);
        if excess == 0 {
            return Ok(0);
        }
        Ok(conn.execute(
            "DELETE FROM recipe_tombstones WHERE hash IN (
                 SELECT hash FROM recipe_tombstones
                 ORDER BY (reason IN ('rejected', 'admin')) ASC, at ASC, hash ASC LIMIT ?)",
            params![excess as i64],
        )?)
    }

    /// These steps produced a video MCR rejected on this domain.
    fn is_barred(conn: &Connection, steps_hash: &str, domain: &str) -> Result<bool> {
        Ok(conn
            .query_row(
                "SELECT 1 FROM recipe_bars WHERE steps_hash = ? AND domain = ?",
                params![steps_hash, domain],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Keep at most `limit` bars, oldest out.
    fn trim_bars(conn: &Connection, limit: usize) -> Result<usize> {
        let excess = count(conn, "SELECT COUNT(*) FROM recipe_bars")?.saturating_sub(limit);
        if excess == 0 {
            return Ok(0);
        }
        Ok(conn.execute(
            "DELETE FROM recipe_bars WHERE rowid IN (
                 SELECT rowid FROM recipe_bars ORDER BY at ASC, steps_hash ASC, domain ASC LIMIT ?)",
            params![excess as i64],
        )?)
    }

    fn write_bar(conn: &Connection, steps_hash: &str, domain: &str, now: DateTime<Utc>) -> Result<()> {
        conn.execute(
            "INSERT INTO recipe_bars (steps_hash, domain, at) VALUES (?, ?, ?)
             ON CONFLICT(steps_hash, domain) DO UPDATE SET at = excluded.at",
            params![steps_hash, domain, ts(now)],
        )?;
        trim_bars(conn, rs::BAR_CAP)?;
        Ok(())
    }

    /// Remember a hash. Write it BEFORE deleting the recipe: the recipe's steps and
    /// scope are copied onto the tombstone so a rejection can still be matched
    /// against other recipes with the same steps.
    fn write_tombstone(conn: &Connection, hash: &str, reason: RetiredReason, now: DateTime<Utc>) -> Result<()> {
        conn.execute(
            "INSERT INTO recipe_tombstones (hash, reason, at) VALUES (?, ?, ?)
             ON CONFLICT(hash) DO UPDATE SET reason = excluded.reason, at = excluded.at",
            params![hash, reason.as_str(), ts(now)],
        )?;
        conn.execute(
            "UPDATE recipe_tombstones SET
                 steps_hash = COALESCE((SELECT steps_hash FROM recipes WHERE hash = ?1), steps_hash),
                 scope_kind = COALESCE((SELECT scope_kind FROM recipes WHERE hash = ?1), scope_kind),
                 scope_key = COALESCE((SELECT scope_key FROM recipes WHERE hash = ?1), scope_key)
             WHERE hash = ?1",
            params![hash],
        )?;
        trim_tombstones(conn, rs::TOMBSTONE_CAP)?;
        Ok(())
    }

    /// Delete site rows until at most `limit` remain: the oldest by last event,
    /// suspended rows last (a suspension is the memory that stops a retry).
    fn shed_site_rows(conn: &Connection, limit: usize) -> Result<usize> {
        let mut removed = 0;
        while count(conn, "SELECT COUNT(*) FROM recipe_sites")? > limit {
            let victim: Option<(i64, String)> = conn
                .query_row(
                    "SELECT recipe_id, domain FROM recipe_sites
                     ORDER BY (state = 'suspended') ASC, scored_at ASC, recipe_id ASC, domain ASC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((id, domain)) = victim else { break };
            conn.execute("DELETE FROM recipe_sites WHERE recipe_id = ? AND domain = ?", params![id, domain])?;
            recompute_state(conn, id)?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Retire the lowest-ranked evictable recipe in `scope` (or anywhere).
    /// `llm` goes before `admin`, and `admin` only for an `admin` insert.
    fn evict_one(
        conn: &Connection,
        scope: Option<(ScopeKind, &str)>,
        inserting: Origin,
        protect: &[i64],
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let kind = scope.map(|(k, _)| k.as_str());
        let key = scope.map(|(_, k)| k);
        let mut rows: HashMap<i64, Vec<SiteRow>> = HashMap::new();
        {
            let mut stmt = conn.prepare(concat!(
                "SELECT recipe_id, ", site_cols!(), " FROM recipe_sites WHERE recipe_id IN (
                     SELECT id FROM recipes WHERE state <> 'retired' AND origin <> 'shipped'
                       AND (?1 IS NULL OR (scope_kind = ?1 AND scope_key = ?2)))"
            ))?;
            let it = stmt.query_map(params![kind, key], |r| Ok((r.get::<_, i64>(0)?, site_row_at(r, 1)?)))?;
            for item in it {
                let (id, row) = item?;
                rows.entry(id).or_default().push(row);
            }
        }
        let mut stmt = conn.prepare(
            "SELECT id, hash, origin FROM recipes WHERE state <> 'retired' AND origin <> 'shipped'
               AND (?1 IS NULL OR (scope_kind = ?1 AND scope_key = ?2))",
        )?;
        let cands = stmt
            .query_map(params![kind, key], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut llm: Vec<(i64, RankKey)> = Vec::new();
        let mut admin: Vec<(i64, RankKey)> = Vec::new();
        for (id, hash, origin) in cands.into_iter().filter(|(id, _, _)| !protect.contains(id)) {
            let mine = rows.get(&id).map(Vec::as_slice).unwrap_or(&[]);
            let k = RankKey {
                rank: rs::pooled_rank(mine, now),
                last_ok_at: mine.iter().filter_map(|r| r.last_ok_at).max(),
                hash,
            };
            match Origin::parse(&origin) {
                Some(Origin::Admin) if inserting == Origin::Admin => admin.push((id, k)),
                Some(Origin::Admin) => {}
                _ => llm.push((id, k)),
            }
        }
        let mut pool = if llm.is_empty() { admin } else { llm };
        rs::rank_order(&mut pool, |(_, k)| k.clone());
        match pool.last() {
            Some((id, _)) => {
                retire(conn, *id, RetiredReason::Evicted, now)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Scope cap, then the global live cap. False: nothing could be evicted.
    fn make_room(conn: &Connection, kind: ScopeKind, key: &str, inserting: Origin, protect: &[i64], now: DateTime<Utc>) -> Result<bool> {
        if kind != ScopeKind::Generic {
            while count_scope(conn, kind, key)? >= rs::SCOPE_CAP {
                if !evict_one(conn, Some((kind, key)), inserting, protect, now)? {
                    return Ok(false);
                }
            }
        }
        while count(
            conn,
            "SELECT COUNT(*) FROM recipes WHERE state <> 'retired' AND origin <> 'shipped'",
        )? >= rs::NONSHIPPED_LIVE_CAP
        {
            if !evict_one(conn, None, inserting, protect, now)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn count_scope(conn: &Connection, kind: ScopeKind, key: &str) -> Result<usize> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM recipes WHERE state <> 'retired' AND origin <> 'shipped'
               AND scope_kind = ? AND scope_key = ?",
            params![kind.as_str(), key],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    /// Room for one more non-shipped row: delete the oldest-retired ones (never
    /// those in `protect`), each leaving a tombstone with its reason. False:
    /// there was nothing to delete.
    fn make_row_room(conn: &Connection, protect: &[i64], now: DateTime<Utc>) -> Result<bool> {
        while count(conn, "SELECT COUNT(*) FROM recipes WHERE origin <> 'shipped'")? >= rs::NONSHIPPED_ROW_CAP {
            let mut stmt = conn.prepare(
                "SELECT id, hash, retired_reason FROM recipes WHERE origin <> 'shipped' AND state = 'retired'
                 ORDER BY retired_at ASC, id ASC",
            )?;
            let all = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let Some((id, hash, reason)) = all.into_iter().find(|(id, _, _)| !protect.contains(id)) else {
                return Ok(false);
            };
            let reason = reason.as_deref().and_then(RetiredReason::parse).unwrap_or(RetiredReason::Evicted);
            write_tombstone(conn, &hash, reason, now)?;
            delete_recipe(conn, id)?;
        }
        Ok(true)
    }

    /// An admin re-adding a recipe makes it an admin recipe, so a later llm insert
    /// cannot evict what an admin just chose.
    fn upgrade_origin(conn: &Connection, id: i64, inserting: Origin) -> Result<()> {
        if inserting == Origin::Admin {
            conn.execute("UPDATE recipes SET origin = 'admin' WHERE id = ? AND origin = 'llm'", params![id])?;
        }
        Ok(())
    }

    /// The insert rules, in order. The caller commits unless it is `OverCap` or
    /// `Refused`. `protect`: recipes the row cap must not delete.
    fn insert_in_tx(conn: &Connection, new: &NewRecipe, protect: &[i64], now: DateTime<Utc>) -> Result<InsertOutcome> {
        // b. a tombstone that forbids a return; any other is stale
        let tomb: Option<String> = conn
            .query_row("SELECT reason FROM recipe_tombstones WHERE hash = ?", params![new.hash], |r| r.get(0))
            .optional()?;
        if let Some(reason) = tomb {
            if RetiredReason::parse(&reason).map_or(false, RetiredReason::refuses_return) {
                return Ok(InsertOutcome::Refused(reason));
            }
            conn.execute("DELETE FROM recipe_tombstones WHERE hash = ?", params![new.hash])?;
        }
        // b2. a stored recipe retired as rejected or by an admin: before anything can merge it away
        let existing = head_by_hash(conn, &new.hash)?;
        if let Some(reason) = existing
            .as_ref()
            .filter(|h| h.state == RecipeState::Retired)
            .and_then(|h| h.retired_reason)
            .filter(|r| r.refuses_return())
        {
            return Ok(InsertOutcome::Refused(reason.as_str().to_string()));
        }
        // b3. MCR rejected these steps on this very site: no site recipe may carry them again
        if new.scope_kind == ScopeKind::Site && is_barred(conn, &new.steps_hash, &new.scope_key)? {
            return Ok(InsertOutcome::Refused("rejected".to_string()));
        }
        // c. a player recipe with these steps already covers every site of the family
        if new.scope_kind == ScopeKind::Site {
            if let Some(family) = new.family.as_deref() {
                let id: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM recipes WHERE scope_kind = 'player' AND scope_key = ? AND steps_hash = ?
                           AND state <> 'retired' ORDER BY id LIMIT 1",
                        params![family, new.steps_hash],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(id) = id {
                    upgrade_origin(conn, id, new.origin)?;
                    return Ok(InsertOutcome::Merged(id));
                }
            }
        }
        // d. the same recipe again
        if let Some(head) = existing {
            if head.state != RecipeState::Retired {
                if let Some(f) = new.family.as_deref() {
                    conn.execute("UPDATE recipes SET family = ? WHERE id = ? AND family IS NULL", params![f, head.id])?;
                }
                upgrade_origin(conn, head.id, new.origin)?;
                return Ok(InsertOutcome::Merged(head.id));
            }
            if !make_room(conn, head.scope_kind, &head.scope_key, new.origin, protect, now)? {
                return Ok(InsertOutcome::OverCap);
            }
            conn.execute(
                "UPDATE recipes SET state = 'candidate', retired_at = NULL, retired_reason = NULL, revived_at = ? WHERE id = ?",
                params![ts(now), head.id],
            )?;
            conn.execute(
                "UPDATE recipe_sites SET state = 'candidate', suspended_reason = NULL, suspended_at = NULL,
                        consecutive_fail = 0
                 WHERE recipe_id = ? AND state = 'suspended' AND suspended_reason = 'failing'",
                params![head.id],
            )?;
            upgrade_origin(conn, head.id, new.origin)?;
            return Ok(InsertOutcome::Revived(head.id));
        }
        // e. a new recipe
        if !make_room(conn, new.scope_kind, &new.scope_key, new.origin, protect, now)? || !make_row_room(conn, protect, now)? {
            return Ok(InsertOutcome::OverCap);
        }
        conn.execute(
            "INSERT INTO recipes (hash, steps_hash, scope_kind, scope_key, family, recipe_json, dsl_version, origin,
                                  state, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'candidate', ?)",
            params![
                new.hash,
                new.steps_hash,
                new.scope_kind.as_str(),
                new.scope_key,
                new.family,
                new.recipe_json,
                new.dsl_version,
                new.origin.as_str(),
                ts(now),
            ],
        )?;
        Ok(InsertOutcome::Inserted(conn.last_insert_rowid()))
    }

    /// One scoring event, inside the caller's transaction (the row is created when missing).
    fn event_in_tx(conn: &Connection, recipe_id: i64, domain: &str, event: Event, now: DateTime<Utc>) -> Result<SiteRow> {
        if domain.is_empty() {
            anyhow::bail!("a recipe event needs a domain");
        }
        let Some(head) = head_by_id(conn, recipe_id)? else {
            anyhow::bail!("no recipe #{recipe_id}");
        };
        let current = load_rows(conn, recipe_id)?.into_iter().find(|(d, _)| d == domain).map(|(_, r)| r);
        let row = match current {
            Some(r) => r,
            None => {
                shed_site_rows(conn, rs::SITE_ROW_CAP - 1)?;
                SiteRow::new(now)
            }
        };
        let next = rs::apply(&row, event, now);
        if event == Event::Rejection {
            write_bar(conn, &head.steps_hash, domain, now)?;
        }
        store_row(conn, recipe_id, domain, &next)?;
        recompute_state(conn, recipe_id)?;
        Ok(next)
    }

    /// Retire a recipe for good and remember its hash, inside the caller's transaction.
    fn tombstone_in_tx(conn: &Connection, hash: &str, reason: RetiredReason, now: DateTime<Utc>) -> Result<()> {
        let head = head_by_hash(conn, hash)?;
        if let Some(h) = &head {
            if h.origin == Origin::Shipped {
                anyhow::bail!("a shipped recipe cannot be tombstoned");
            }
            if matches!(reason, RetiredReason::Admin | RetiredReason::Rejected) && h.scope_kind == ScopeKind::Site {
                write_bar(conn, &h.steps_hash, &h.scope_key, now)?;
            }
        }
        write_tombstone(conn, hash, reason, now)?;
        if let Some(h) = head {
            if h.state == RecipeState::Retired {
                conn.execute("UPDATE recipes SET retired_reason = ? WHERE id = ?", params![reason.as_str(), h.id])?;
            } else {
                retire(conn, h.id, reason, now)?;
            }
        }
        Ok(())
    }

    /// What MCR's decision on a held video teaches the recipe that found it
    /// (P10.13), in the decision's own transaction: an approval is an
    /// `Approval` on that domain; «Λάθος βίντεο» is a `Rejection` there (it
    /// suspends the recipe and bars its steps) and, for a recipe the model
    /// wrote that MCR never approved anywhere, a tombstone so it cannot come
    /// back. A recipe that is gone teaches nothing (the decision still stands).
    pub(super) fn learn_from_mcr(
        conn: &Connection,
        recipe_id: i64,
        recipe_hash: Option<&str>,
        domain: &str,
        approved: bool,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if domain.is_empty() {
            return Ok(());
        }
        let Some(head) = head_by_id(conn, recipe_id)? else { return Ok(()) };
        // Ids are reused after a delete: the attempt's hash must be the recipe's, or it is another one.
        if recipe_hash != Some(head.hash.as_str()) {
            return Ok(());
        }
        if approved {
            event_in_tx(conn, recipe_id, domain, Event::Approval, now)?;
            return Ok(());
        }
        event_in_tx(conn, recipe_id, domain, Event::Rejection, now)?;
        let approvals: i64 =
            conn.query_row("SELECT COALESCE(SUM(approvals), 0) FROM recipe_sites WHERE recipe_id = ?", params![recipe_id], |r| r.get(0))?;
        if head.origin == Origin::Llm && approvals == 0 {
            tombstone_in_tx(conn, &head.hash, RetiredReason::Rejected, now)?;
        }
        Ok(())
    }

    fn check_new(new: &NewRecipe) -> Result<()> {
        if new.recipe_json.len() > rs::MAX_RECIPE_JSON {
            anyhow::bail!("recipe is {} bytes, the limit is {}", new.recipe_json.len(), rs::MAX_RECIPE_JSON);
        }
        if new.origin == Origin::Shipped {
            anyhow::bail!("shipped recipes are added with sync_shipped");
        }
        if new.scope_kind == ScopeKind::Generic {
            anyhow::bail!("only shipped recipes have the generic scope");
        }
        if new.hash.is_empty() || new.steps_hash.is_empty() || new.scope_key.is_empty() {
            anyhow::bail!("a recipe needs a hash, a steps hash and a scope key");
        }
        Ok(())
    }

    impl Repository {
        /// Add a recipe, or find the one it already is. See `InsertOutcome`.
        pub fn insert_recipe(&self, new: &NewRecipe, now: DateTime<Utc>) -> Result<InsertOutcome> {
            check_new(new)?;
            let mut conn = self.pool.get()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let out = insert_in_tx(&tx, new, &[], now)?;
            match out {
                InsertOutcome::OverCap | InsertOutcome::Refused(_) => drop(tx),
                _ => tx.commit()?,
            }
            Ok(out)
        }

        /// MCR rejected a video these steps produced on `domain`: no recipe
        /// with them is offered or stored for it again.
        pub fn recipe_steps_barred(&self, steps_hash: &str, domain: &str) -> Result<bool> {
            let conn = self.pool.get()?;
            is_barred(&conn, steps_hash, domain)
        }

        /// Score one event for a recipe on a domain; the row is created when missing.
        pub fn recipe_event(&self, recipe_id: i64, domain: &str, event: Event, now: DateTime<Utc>) -> Result<SiteRow> {
            let mut conn = self.pool.get()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let next = event_in_tx(&tx, recipe_id, domain, event, now)?;
            tx.commit()?;
            Ok(next)
        }

        /// The recipes to try for a page of `domain`, best first: this site's
        /// proven recipes, its untried ones, the player family's, then the
        /// shipped generic ones. Never retired, never suspended on `domain`.
        pub fn recipes_for_site(
            &self,
            domain: &str,
            family: Option<&str>,
            now: DateTime<Utc>,
        ) -> Result<Vec<RankedRecipe>> {
            self.ranked_recipes(domain, family, now, false)
        }

        /// The recipes the overnight self-heal tries on a site's canary pages
        /// (P10.14): [`Self::recipes_for_site`] plus every recipe (site, player or
        /// shipped) suspended here as `failing`, so a recipe the site outgrew can
        /// earn its way back; the canary misses that suspend one fast are the same
        /// events that lift it. Never one suspended as `rejected`
        /// (MCR said it gave a wrong video), never barred steps, never retired.
        pub fn recipes_for_canary(
            &self,
            domain: &str,
            family: Option<&str>,
            now: DateTime<Utc>,
        ) -> Result<Vec<RankedRecipe>> {
            self.ranked_recipes(domain, family, now, true)
        }

        fn ranked_recipes(
            &self,
            domain: &str,
            family: Option<&str>,
            now: DateTime<Utc>,
            with_failing: bool,
        ) -> Result<Vec<RankedRecipe>> {
            let conn = self.pool.get()?;
            let mut stmt = conn.prepare(concat!(
                "SELECT ", head_cols!(), ", recipe_json, dsl_version FROM recipes
                 WHERE state <> 'retired' AND ((scope_kind = 'site' AND scope_key = ?1)
                    OR (scope_kind = 'player' AND ?2 IS NOT NULL AND scope_key = ?2)
                    OR scope_kind = 'generic')"
            ))?;
            let found = stmt
                .query_map(params![domain, family], |r| Ok((head_from_row(r)?, r.get::<_, String>(12)?, r.get::<_, u32>(13)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let prior = rs::lcb(1.0, 1.0);
            let mut out: Vec<(u8, RankKey, RankedRecipe)> = Vec::new();
            for (head, json, dsl) in found {
                let rows = load_rows(&conn, head.id)?;
                let mine = rows.iter().find(|(d, _)| d == domain).map(|(_, r)| r);
                let retry = with_failing && mine.is_some_and(|r| r.suspended_reason == Some(SuspendReason::Failing));
                if mine.map_or(false, SiteRow::is_suspended) && !retry {
                    continue;
                }
                // Steps MCR rejected on this domain stay barred, whatever recipe carries them.
                if is_barred(&conn, &head.steps_hash, domain)? {
                    continue;
                }
                let active = mine.map_or(false, |r| r.state == SiteState::Active);
                let tier = match head.scope_kind {
                    ScopeKind::Site if active => 1,
                    ScopeKind::Site => 2,
                    ScopeKind::Player if active => 3,
                    ScopeKind::Player => 4,
                    ScopeKind::Generic => 5,
                };
                let rank = match (mine, head.scope_kind) {
                    (Some(r), _) => rs::site_rank(r, now),
                    (None, ScopeKind::Player) => {
                        let all: Vec<SiteRow> = rows.iter().map(|(_, r)| r.clone()).collect();
                        rs::pooled_rank(&all, now)
                    }
                    (None, _) => prior,
                };
                let last_ok_at = match mine {
                    Some(r) => r.last_ok_at,
                    None => rows.iter().filter_map(|(_, r)| r.last_ok_at).max(),
                };
                let key = RankKey { rank, last_ok_at, hash: head.hash.clone() };
                out.push((
                    tier,
                    key,
                    RankedRecipe {
                        id: head.id,
                        hash: head.hash,
                        steps_hash: head.steps_hash,
                        scope_kind: head.scope_kind,
                        scope_key: head.scope_key,
                        origin: head.origin,
                        recipe_json: json,
                        dsl_version: dsl,
                        site_state: mine.map(|r| r.state),
                        rank,
                    },
                ));
            }
            out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| rs::rank_cmp(&a.1, &b.1)));
            Ok(out.into_iter().map(|(_, _, r)| r).collect())
        }

        /// Retire a recipe for good and remember its hash (MCR / admin use).
        /// A shipped recipe is never tombstoned.
        pub fn tombstone_recipe(&self, hash: &str, reason: RetiredReason, now: DateTime<Utc>) -> Result<()> {
            let mut conn = self.pool.get()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tombstone_in_tx(&tx, hash, reason, now)?;
            tx.commit()?;
            Ok(())
        }

        /// Replace the site recipes that share `steps_hash` by one player recipe
        /// when two different sites of `family` have proven them. The caller
        /// supplies the player recipe and its hash. `Ok(None)`: nothing to do,
        /// or the player recipe may not exist (refused or over cap).
        pub fn promote_to_player(
            &self,
            steps_hash: &str,
            family: &str,
            player_hash: &str,
            player_json: &str,
            dsl_version: u32,
            now: DateTime<Utc>,
        ) -> Result<Option<i64>> {
            let mut conn = self.pool.get()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let sources: Vec<PromoSource> = {
                let mut stmt = tx.prepare(
                    "SELECT r.id, r.scope_key, r.family, r.origin,
                            EXISTS (SELECT 1 FROM recipe_sites s
                                    WHERE s.recipe_id = r.id AND s.domain = r.scope_key AND s.state = 'active')
                     FROM recipes r
                     WHERE r.scope_kind = 'site' AND r.state <> 'retired' AND r.steps_hash = ?
                     ORDER BY r.id",
                )?;
                let rows = stmt
                    .query_map(params![steps_hash], |r| {
                        let origin: String = r.get(3)?;
                        Ok(PromoSource {
                            id: r.get(0)?,
                            scope_key: r.get(1)?,
                            family: r.get(2)?,
                            origin: Origin::parse(&origin).unwrap_or(Origin::Llm),
                            own_active: r.get(4)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            let Some(plan) = rs::plan_promotion(&sources, family) else { return Ok(None) };
            let new = NewRecipe {
                scope_kind: ScopeKind::Player,
                scope_key: family.to_string(),
                family: Some(family.to_string()),
                recipe_json: player_json.to_string(),
                hash: player_hash.to_string(),
                steps_hash: steps_hash.to_string(),
                dsl_version,
                origin: plan.origin,
            };
            check_new(&new)?;
            // The sources go first: they free the room the player recipe needs.
            for id in &plan.source_ids {
                retire(&tx, *id, RetiredReason::Superseded, now)?;
            }
            let player = match insert_in_tx(&tx, &new, &plan.source_ids, now)? {
                InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) | InsertOutcome::Revived(id) => id,
                InsertOutcome::Refused(_) | InsertOutcome::OverCap => return Ok(None),
            };
            for id in &plan.source_ids {
                for (domain, row) in load_rows(&tx, *id)? {
                    // A rejection stays with its retired source: the bar keeps P off that
                    // domain, and P must not look rejected because of it.
                    if row.state == SiteState::Suspended && row.suspended_reason == Some(SuspendReason::Rejected) {
                        continue;
                    }
                    let existing = load_rows(&tx, player)?.into_iter().find(|(d, _)| *d == domain).map(|(_, r)| r);
                    let moved = match existing {
                        Some(have) => rs::merge_rows(&have, &row, now),
                        None => row,
                    };
                    store_row(&tx, player, &domain, &moved)?;
                    tx.execute("DELETE FROM recipe_sites WHERE recipe_id = ? AND domain = ?", params![id, domain])?;
                }
            }
            recompute_state(&tx, player)?;
            tx.commit()?;
            Ok(Some(player))
        }

        /// Make the shipped generic recipes in the store equal `list`.
        /// Returns (added, removed). No cap and no tombstone applies.
        pub fn sync_shipped(&self, list: &[ShippedRecipe], now: DateTime<Utc>) -> Result<(usize, usize)> {
            let mut conn = self.pool.get()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let have: Vec<(i64, String)> = {
                let mut stmt = tx.prepare("SELECT id, hash FROM recipes WHERE origin = 'shipped'")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            let mut removed = 0;
            for (id, hash) in &have {
                if !list.iter().any(|s| &s.hash == hash) {
                    delete_recipe(&tx, *id)?;
                    removed += 1;
                }
            }
            let mut added = 0;
            for s in list {
                if have.iter().any(|(_, h)| *h == s.hash) {
                    continue;
                }
                added += tx.execute(
                    "INSERT OR IGNORE INTO recipes (hash, steps_hash, scope_kind, scope_key, family, recipe_json,
                                                    dsl_version, origin, state, created_at)
                     VALUES (?, ?, 'generic', ?, NULL, ?, ?, 'shipped', 'active', ?)",
                    params![s.hash, s.steps_hash, s.name, s.recipe_json, s.dsl_version, ts(now)],
                )?;
            }
            tx.commit()?;
            Ok((added, removed))
        }

        /// Nightly upkeep: forget stale scores, retire what was not used or keeps
        /// failing, delete what has been retired long enough, expire tombstones,
        /// and hold the caps.
        pub fn recipe_gc(&self, now: DateTime<Utc>) -> Result<RecipeGcReport> {
            let mut conn = self.pool.get()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut rep = RecipeGcReport::default();

            // a. scores nobody has touched for half a year
            let stale = ts(now - Duration::days(rs::STALE_ROW_DAYS));
            let touched: Vec<i64> = {
                let mut stmt = tx.prepare("SELECT DISTINCT recipe_id FROM recipe_sites WHERE scored_at <= ?
                     AND NOT (state = 'suspended' AND suspended_reason = 'rejected')")?;
                let ids = stmt.query_map(params![stale], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                ids
            };
            rep.stale_rows = tx.execute(
                "DELETE FROM recipe_sites WHERE scored_at <= ?
                   AND NOT (state = 'suspended' AND suspended_reason = 'rejected')",
                params![stale],
            )?;
            for id in touched {
                recompute_state(&tx, id)?;
            }

            // b. time-driven retirement
            let live: Vec<Head> = {
                let mut stmt = tx.prepare(concat!(
                    "SELECT ", head_cols!(), " FROM recipes WHERE state <> 'retired' AND origin <> 'shipped'"
                ))?;
                let heads = stmt.query_map([], head_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                heads
            };
            for h in live {
                let rows: Vec<SiteRow> = load_rows(&tx, h.id)?.into_iter().map(|(_, r)| r).collect();
                let meta = RecipeMeta { origin: h.origin, state: h.state, created_at: h.created_at, revived_at: h.revived_at };
                if let Some(reason) = rs::due_retirement(&meta, &rows, now) {
                    retire(&tx, h.id, reason, now)?;
                    match reason {
                        RetiredReason::Idle => rep.retired_idle += 1,
                        RetiredReason::Failing => rep.retired_failing += 1,
                        _ => rep.retired_rejected += 1,
                    }
                }
            }

            // c. retired long enough: gone, but remembered under its reason
            let old: Vec<(i64, String, Option<String>)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, hash, retired_reason FROM recipes
                     WHERE origin <> 'shipped' AND state = 'retired' AND retired_at <= ?
                     ORDER BY retired_at, id",
                )?;
                let rows = stmt
                    .query_map(params![ts(now - Duration::days(rs::RETIRED_KEEP_DAYS))], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            for (id, hash, reason) in old {
                let reason = reason.as_deref().and_then(RetiredReason::parse).unwrap_or(RetiredReason::Evicted);
                write_tombstone(&tx, &hash, reason, now)?;
                delete_recipe(&tx, id)?;
                rep.deleted_recipes += 1;
            }

            // d. tombstones expire; e. and are capped
            rep.tombstones_expired = tx.execute(
                "DELETE FROM recipe_tombstones WHERE at <= ?",
                params![ts(now - Duration::days(rs::TOMBSTONE_DAYS))],
            )?;
            rep.tombstones_trimmed = trim_tombstones(&tx, rs::TOMBSTONE_CAP)?;
            rep.bars_expired = tx.execute(
                "DELETE FROM recipe_bars WHERE at <= ?",
                params![ts(now - Duration::days(rs::BAR_DAYS))],
            )?;
            rep.bars_trimmed = trim_bars(&tx, rs::BAR_CAP)?;

            // f. the site-row cap
            rep.rows_trimmed = shed_site_rows(&tx, rs::SITE_ROW_CAP)?;
            tx.commit()?;
            Ok(rep)
        }

        /// Every recipe, shipped and retired too, with its per-site scores, newest
        /// first (the admin's Recipes card, P10.15a).
        pub fn list_recipes(&self, now: DateTime<Utc>) -> Result<Vec<super::RecipeListRow>> {
            let conn = self.pool.get()?;
            let mut stmt = conn.prepare(concat!(
                "SELECT ", head_cols!(), ", recipe_json FROM recipes ORDER BY id DESC"
            ))?;
            let heads = stmt
                .query_map([], |r| Ok((head_from_row(r)?, r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(9)?, r.get::<_, String>(12)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::with_capacity(heads.len());
            for (h, family, retired_at, json) in heads {
                let rows = load_rows(&conn, h.id)?;
                let all: Vec<SiteRow> = rows.iter().map(|(_, r)| r.clone()).collect();
                out.push(super::RecipeListRow {
                    id: h.id,
                    hash: h.hash,
                    scope_kind: h.scope_kind.as_str().to_string(),
                    scope_key: h.scope_key,
                    family,
                    origin: h.origin.as_str().to_string(),
                    state: h.state.as_str().to_string(),
                    created_at: Some(h.created_at),
                    retired_at: parse_opt(retired_at),
                    retired_reason: h.retired_reason.map(|r| r.as_str().to_string()),
                    recipe_json: json,
                    rank: rs::pooled_rank(&all, now),
                    sites: rows
                        .into_iter()
                        .map(|(domain, r)| super::RecipeSiteView {
                            domain,
                            state: r.state.as_str().to_string(),
                            suspended_reason: r.suspended_reason.map(|x| x.as_str().to_string()),
                            rank: rs::site_rank(&r, now),
                            approvals: r.approvals,
                            rejections: r.rejections,
                            uses: r.uses,
                            last_used_at: r.last_used_at,
                            last_ok_at: r.last_ok_at,
                        })
                        .collect(),
                });
            }
            Ok(out)
        }

        /// The recipes an admin may carry to another installation: live, and written
        /// by an admin or the model. Never a shipped one (the program carries those).
        pub fn export_recipes(&self) -> Result<Vec<super::RecipeExportRow>> {
            let conn = self.pool.get()?;
            let mut stmt = conn.prepare(
                "SELECT scope_kind, scope_key, family, recipe_json FROM recipes
                 WHERE state <> 'retired' AND origin IN ('admin', 'llm')
                   AND NOT (scope_kind = 'site' AND EXISTS
                        (SELECT 1 FROM recipe_bars b WHERE b.steps_hash = recipes.steps_hash AND b.domain = recipes.scope_key))
                 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(super::RecipeExportRow {
                        scope_kind: r.get(0)?,
                        scope_key: r.get(1)?,
                        family: r.get(2)?,
                        recipe_json: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }

        /// An admin's import, all in ONE immediate transaction: any `Err` leaves
        /// nothing stored. An entry never evicts another entry of the same batch;
        /// when there is no other room it is `OverCap`. An entry that is refused or
        /// over the cap leaves no trace (its own savepoint is rolled back). Each
        /// result names the recipes its insert evicted.
        pub fn import_recipes(&self, entries: &[NewRecipe], now: DateTime<Utc>) -> Result<Vec<super::RecipeImportResult>> {
            let mut conn = self.pool.get()?;
            let mut tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut mine: Vec<i64> = Vec::new();
            let mut out = Vec::with_capacity(entries.len());
            let live = |c: &Connection| -> Result<Vec<(i64, String, String)>> {
                let mut stmt = c.prepare(
                    "SELECT id, scope_kind, scope_key FROM recipes WHERE state <> 'retired' AND origin <> 'shipped'",
                )?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            };
            for new in entries {
                check_new(new)?;
                let before = live(&tx)?;
                let sp = tx.savepoint()?;
                let outcome = insert_in_tx(&sp, new, &mine, now)?;
                let kept = !matches!(outcome, InsertOutcome::OverCap | InsertOutcome::Refused(_));
                let mut evicted = Vec::new();
                if kept {
                    let after: Vec<i64> = live(&sp)?.into_iter().map(|(id, _, _)| id).collect();
                    for (id, kind, key) in before {
                        if !after.contains(&id) {
                            let same = kind == new.scope_kind.as_str() && key == new.scope_key;
                            evicted.push(super::EvictedRecipe {
                                id,
                                scope_kind: kind,
                                scope_key: key,
                                why: if same { "its scope was full" } else { "the store was full" }.to_string(),
                            });
                        }
                    }
                    sp.commit()?;
                }
                if let InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) | InsertOutcome::Revived(id) = outcome {
                    mine.push(id);
                }
                out.push(super::RecipeImportResult { outcome, evicted });
            }
            tx.commit()?;
            Ok(out)
        }

        /// An admin switches a recipe off on one site. It is the same suspension a
        /// run of failures makes (`failing`), so two canary passes in a row lift it
        /// again, and so does a `failing` suspension's 30-day retirement. `false`:
        /// no such live recipe.
        pub fn admin_suspend_recipe(&self, id: i64, domain: &str, now: DateTime<Utc>) -> Result<bool> {
            let domain = domain.trim().to_lowercase();
            if domain.is_empty() {
                anyhow::bail!("a suspension needs a domain");
            }
            let mut conn = self.pool.get()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            match head_by_id(&tx, id)? {
                Some(h) if h.state != RecipeState::Retired => {}
                _ => return Ok(false),
            }
            let current = load_rows(&tx, id)?.into_iter().find(|(d, _)| *d == domain).map(|(_, r)| r);
            let mut row = match current {
                Some(r) => r,
                None => {
                    shed_site_rows(&tx, rs::SITE_ROW_CAP - 1)?;
                    SiteRow::new(now)
                }
            };
            // A rejection's suspension is MCR's word and stays as it is.
            if !(row.state == SiteState::Suspended && row.suspended_reason == Some(SuspendReason::Rejected)) {
                if !row.is_suspended() {
                    row.suspended_at = Some(now);
                }
                row.state = SiteState::Suspended;
                row.suspended_reason = Some(SuspendReason::Failing);
                row.pass_streak = 0;
            }
            store_row(&tx, id, &domain, &row)?;
            recompute_state(&tx, id)?;
            tx.commit()?;
            Ok(true)
        }

        /// An admin retires a recipe for good: its hash is remembered as `admin`, so
        /// it cannot be inserted again, and its steps are barred on its own site.
        pub fn admin_retire_recipe(&self, id: i64, now: DateTime<Utc>) -> Result<super::AdminRetire> {
            use super::AdminRetire;
            let mut conn = self.pool.get()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let Some(head) = head_by_id(&tx, id)? else { return Ok(AdminRetire::NotFound) };
            if head.origin == Origin::Shipped {
                return Ok(AdminRetire::Shipped);
            }
            // Retired as evicted, superseded or failing it could still be revived: re-label it `admin`.
            if head.state == RecipeState::Retired && head.retired_reason.map_or(false, RetiredReason::refuses_return) {
                return Ok(AdminRetire::AlreadyRetired);
            }
            if head.state == RecipeState::Retired {
                // Only the memory that it may not return: no bar, which would stop the same steps
                // on this site even in the recipe that superseded this one.
                write_tombstone(&tx, &head.hash, RetiredReason::Admin, now)?;
                tx.execute("UPDATE recipes SET retired_reason = 'admin' WHERE id = ?", params![id])?;
            } else {
                tombstone_in_tx(&tx, &head.hash, RetiredReason::Admin, now)?;
            }
            tx.commit()?;
            Ok(AdminRetire::Retired)
        }

        pub fn recipe_counts(&self) -> Result<RecipeCounts> {
            let conn = self.pool.get()?;
            Ok(RecipeCounts {
                live: count(&conn, "SELECT COUNT(*) FROM recipes WHERE origin <> 'shipped' AND state <> 'retired'")?,
                retired: count(&conn, "SELECT COUNT(*) FROM recipes WHERE origin <> 'shipped' AND state = 'retired'")?,
                shipped: count(&conn, "SELECT COUNT(*) FROM recipes WHERE origin = 'shipped'")?,
                sites: count(&conn, "SELECT COUNT(*) FROM recipe_sites")?,
                tombstones: count(&conn, "SELECT COUNT(*) FROM recipe_tombstones")?,
                bars: count(&conn, "SELECT COUNT(*) FROM recipe_bars")?,
                json_bytes: count(&conn, "SELECT COALESCE(SUM(LENGTH(recipe_json)), 0) FROM recipes")?,
            })
        }
    }
}

// ---------------------------------------------------------------- benchmark (P6.4)

/// What asking to start a benchmark run got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchStart {
    /// The run was created, `running`.
    Started(i64),
    /// This run is still going; only one at a time.
    Busy(i64),
}

fn bench_target(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::benchmark::TargetRow> {
    let expected: String = r.get(6)?;
    Ok(crate::benchmark::TargetRow {
        id: r.get(0)?,
        idx: r.get(1)?,
        url: r.get(2)?,
        domain: r.get(3)?,
        category: r.get(4)?,
        friction: r.get(5)?,
        expected: crate::benchmark::Expected::parse(&expected).unwrap_or(crate::benchmark::Expected::Pass),
        note: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
    })
}

fn bench_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::benchmark::RunRow> {
    let mode: String = r.get(5)?;
    Ok(crate::benchmark::RunRow {
        id: r.get(0)?,
        started_at: timestamps::parse_opt(r.get(1)?),
        finished_at: timestamps::parse_opt(r.get(2)?),
        heartbeat_at: timestamps::parse_opt(r.get(3)?),
        paused: r.get::<_, i64>(4)? != 0,
        mode: crate::benchmark::Mode::parse(&mode).unwrap_or(crate::benchmark::Mode::Extract),
        state: r.get(6)?,
        params_json: r.get(7)?,
        summary_json: r.get(8)?,
        owner: r.get(9)?,
    })
}

const BENCH_RUN_COLUMNS: &str = "id, started_at, finished_at, heartbeat_at, paused, mode, state, params_json, summary_json, owner";

impl Repository {
    /// Store the corpus: one row per link, replacing the text of a row that exists
    /// (the number is the key), so running it again changes nothing.
    pub fn seed_benchmark_targets(&self) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        for row in crate::benchmark::CORPUS {
            tx.execute(
                "INSERT INTO benchmark_targets (idx, url, domain, category, friction, expected, note)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(idx) DO UPDATE SET url = excluded.url, domain = excluded.domain,
                   category = excluded.category, friction = excluded.friction,
                   expected = excluded.expected, note = excluded.note",
                params![row.idx, row.url, row.domain, row.category, row.friction, row.expected.as_str(), row.note],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn benchmark_targets(&self) -> Result<Vec<crate::benchmark::TargetRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, idx, url, domain, category, friction, expected, note FROM benchmark_targets ORDER BY idx",
        )?;
        let rows = stmt.query_map([], bench_target)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Open a run, unless one is going. A `running` run older than `stale_after` was
    /// left by a process that died: it is marked `abandoned` first and does not block.
    pub fn begin_benchmark_run(
        &self,
        mode: crate::benchmark::Mode,
        params_json: &str,
        now: chrono::DateTime<Utc>,
        stale_after: Duration,
        owner: &str,
    ) -> Result<BenchStart> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE benchmark_runs SET state = 'abandoned', finished_at = ? WHERE state = 'running' AND heartbeat_at < ?",
            params![timestamps::format(now), timestamps::format(now - stale_after)],
        )?;
        let running: Option<i64> = tx
            .query_row("SELECT id FROM benchmark_runs WHERE state = 'running' ORDER BY id LIMIT 1", [], |r| r.get(0))
            .optional()?;
        if let Some(id) = running {
            return Ok(BenchStart::Busy(id));
        }
        tx.execute(
            "INSERT INTO benchmark_runs (started_at, heartbeat_at, mode, state, params_json, owner) VALUES (?, ?, ?, 'running', ?, ?)",
            params![timestamps::format(now), timestamps::format(now), mode.as_str(), params_json, owner],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(BenchStart::Started(id))
    }

    /// The run is alive: written at start, after each target and while it waits.
    /// A run whose heartbeat stops is abandoned after the stale limit.
    pub fn benchmark_heartbeat(&self, run_id: i64, paused: bool, now: chrono::DateTime<Utc>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE benchmark_runs SET heartbeat_at = ?, paused = ? WHERE id = ? AND state = 'running'",
            params![timestamps::format(now), paused as i64, run_id],
        )?;
        Ok(())
    }

    /// At daemon start-up: every `running` run the daemon owned died with the previous
    /// process, whatever its heartbeat says; a command-line run (another process, perhaps
    /// alive) is abandoned only when its heartbeat is older than `cli_stale_after`.
    /// The number of runs still running afterwards.
    pub fn abandon_stale_benchmark_runs(&self, now: chrono::DateTime<Utc>, cli_stale_after: Duration) -> Result<usize> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE benchmark_runs SET state = 'abandoned', finished_at = ?
             WHERE state = 'running' AND (owner = 'daemon' OR heartbeat_at < ?)",
            params![timestamps::format(now), timestamps::format(now - cli_stale_after)],
        )?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM benchmark_runs WHERE state = 'running'", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Benchmark runs that are `running` now.
    pub fn benchmark_running_count(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM benchmark_runs WHERE state = 'running'", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// One target's outcome; a second write for the same target replaces the first.
    pub fn record_benchmark_result(&self, r: &crate::benchmark::ResultRow) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR REPLACE INTO benchmark_results
               (run_id, target_id, extract_ok, extract_ms, method, candidates, download_ok, download_ms,
                transcode_ms, verify_ok, total_ms, error_code, stream_host)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                r.run_id,
                r.target_id,
                r.extract_ok,
                r.extract_ms,
                r.method,
                r.candidates,
                r.download_ok,
                r.download_ms,
                r.transcode_ms,
                r.verify_ok,
                r.total_ms,
                r.error_code,
                r.stream_host
            ],
        )?;
        Ok(())
    }

    /// End a run that is still `running`: `done` or `failed`, with its summary.
    /// A run already marked `abandoned` stays so.
    pub fn finish_benchmark_run(
        &self,
        run_id: i64,
        state: &str,
        summary_json: Option<&str>,
        now: chrono::DateTime<Utc>,
    ) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE benchmark_runs SET state = ?, finished_at = ?, summary_json = ? WHERE id = ? AND state = 'running'",
            params![state, timestamps::format(now), summary_json, run_id],
        )?;
        Ok(n > 0)
    }

    /// The runs, newest first.
    pub fn benchmark_runs(&self, limit: usize) -> Result<Vec<crate::benchmark::RunRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("SELECT {BENCH_RUN_COLUMNS} FROM benchmark_runs ORDER BY id DESC LIMIT ?"))?;
        let rows = stmt.query_map(params![limit as i64], bench_run)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn benchmark_run(&self, id: i64) -> Result<Option<crate::benchmark::RunRow>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row(&format!("SELECT {BENCH_RUN_COLUMNS} FROM benchmark_runs WHERE id = ?"), params![id], bench_run)
            .optional()?)
    }

    /// The latest finished (`done`) run of `mode` before run `before_id`: what a
    /// report is compared with. A run of the other mode is never returned.
    pub fn previous_benchmark_run(
        &self,
        mode: crate::benchmark::Mode,
        before_id: i64,
    ) -> Result<Option<crate::benchmark::RunRow>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {BENCH_RUN_COLUMNS} FROM benchmark_runs
                     WHERE state = 'done' AND mode = ? AND id < ? ORDER BY id DESC LIMIT 1"
                ),
                params![mode.as_str(), before_id],
                bench_run,
            )
            .optional()?)
    }

    pub fn benchmark_results(&self, run_id: i64) -> Result<Vec<crate::benchmark::ResultRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, target_id, extract_ok, extract_ms, method, candidates, download_ok, download_ms,
                    transcode_ms, verify_ok, total_ms, error_code, stream_host
             FROM benchmark_results WHERE run_id = ? ORDER BY target_id",
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            Ok(crate::benchmark::ResultRow {
                run_id: r.get(0)?,
                target_id: r.get(1)?,
                extract_ok: r.get(2)?,
                extract_ms: r.get(3)?,
                method: r.get(4)?,
                candidates: r.get(5)?,
                download_ok: r.get(6)?,
                download_ms: r.get(7)?,
                transcode_ms: r.get(8)?,
                verify_ok: r.get(9)?,
                total_ms: r.get(10)?,
                error_code: r.get(11)?,
                stream_host: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

// ---------------------------------------------------------------- cookie jars (P3.5)

impl Repository {
    /// Store the jar for `domain`, sealed, replacing any earlier one. The
    /// text is never logged. At most [`crate::cookies::MAX_JARS`] jars: a new
    /// domain beyond that is refused, a replacement is not.
    pub fn put_cookie_jar(
        &self,
        domain: &str,
        jar: &crate::cookies::ParsedJar,
        updated_by: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let sealed = crate::secrets::seal_bytes(jar.text.as_bytes()).context("Could not seal the cookie jar")?;
        let mut conn = self.pool.get()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row("SELECT COUNT(*) FROM cookie_jars WHERE domain = ?", params![domain], |r| r.get::<_, i64>(0))? > 0;
        if !exists {
            let n: i64 = tx.query_row("SELECT COUNT(*) FROM cookie_jars", [], |r| r.get(0))?;
            if n as usize >= crate::cookies::MAX_JARS {
                anyhow::bail!("At most {} cookie jars can be stored.", crate::cookies::MAX_JARS);
            }
        }
        // A new upload starts a new life: the old test result described other cookies.
        // The sample address stays, so the nightly test has something to run.
        tx.execute(
            "INSERT INTO cookie_jars (domain, sealed, cookie_count, expires_min, updated_at, updated_by)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(domain) DO UPDATE SET sealed = excluded.sealed, cookie_count = excluded.cookie_count,
               expires_min = excluded.expires_min, updated_at = excluded.updated_at, updated_by = excluded.updated_by,
               last_test_at = NULL, last_test_ok = NULL, last_test_detail = NULL",
            params![
                domain,
                sealed,
                jar.count as i64,
                jar.expires_min.map(timestamps::format),
                timestamps::format(now),
                updated_by
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The Netscape text of the jar for `domain`, unsealed. `None` when there is
    /// no jar; an error when it cannot be opened (a database moved to another machine).
    pub fn cookie_jar_text(&self, domain: &str) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        let sealed: Option<Vec<u8>> = conn
            .query_row("SELECT sealed FROM cookie_jars WHERE domain = ?", params![domain], |r| r.get(0))
            .optional()?;
        let Some(sealed) = sealed else { return Ok(None) };
        let plain = crate::secrets::unseal_bytes(&sealed).context("Could not open the cookie jar")?;
        let text = String::from_utf8(plain).context("The cookie jar is not text")?;
        // Before the text goes anywhere: registering a value twice changes nothing.
        if let Some(r) = &self.redactions {
            r.extend(crate::cookies::values_of(&text));
        }
        Ok(Some(text))
    }

    /// Whether a jar exists for `domain`, without opening it.
    pub fn has_cookie_jar(&self, domain: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM cookie_jars WHERE domain = ?", params![domain], |r| r.get::<_, i64>(0))? > 0)
    }

    /// Every jar's cookie values, for the log redactor. Jars that cannot be
    /// opened are skipped.
    pub fn cookie_values(&self) -> Vec<String> {
        let domains: Vec<String> = match self.list_cookie_jars() {
            Ok(j) => j.into_iter().map(|j| j.domain).collect(),
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for d in domains {
            if let Ok(Some(text)) = self.cookie_jar_text(&d) {
                out.extend(text.lines().filter(|l| !l.starts_with('#') || l.starts_with("#HttpOnly_")).filter_map(|l| {
                    let v = l.split('\t').nth(6)?;
                    (!v.is_empty()).then(|| v.to_string())
                }));
            }
        }
        out
    }

    /// The jars, without contents, by domain.
    pub fn list_cookie_jars(&self) -> Result<Vec<crate::cookies::CookieJarInfo>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT domain, cookie_count, expires_min, updated_at, updated_by, last_test_at, last_test_ok,
                    last_test_detail, last_sample_url FROM cookie_jars ORDER BY domain",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::cookies::CookieJarInfo {
                domain: r.get(0)?,
                cookie_count: r.get(1)?,
                expires_min: timestamps::parse_opt(r.get(2)?),
                updated_at: timestamps::parse_opt(r.get(3)?),
                updated_by: r.get(4)?,
                last_test_at: timestamps::parse_opt(r.get(5)?),
                last_test_ok: r.get::<_, Option<i64>>(6)?.map(|v| v != 0),
                last_test_detail: r.get(7)?,
                last_sample_url: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_cookie_jar(&self, domain: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        Ok(conn.execute("DELETE FROM cookie_jars WHERE domain = ?", params![domain])? > 0)
    }

    /// The outcome of a test of the jar for `domain`, and the address it used.
    /// `detail` is already scrubbed (see [`crate::cookies::scrub_detail`]).
    pub fn record_cookie_test(
        &self,
        domain: &str,
        ok: bool,
        detail: &str,
        sample_url: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE cookie_jars SET last_test_at = ?, last_test_ok = ?, last_test_detail = ?, last_sample_url = ? WHERE domain = ?",
            params![timestamps::format(now), ok as i64, detail, sample_url, domain],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn evidence_rows_are_recorded_listed_oldest_first_and_deleted() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        assert!(repo.list_evidence()?.is_empty());
        repo.record_evidence(7, "temp/evidence/7.json", 1000)?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        repo.record_evidence(3, "temp/evidence/3.json", 2000)?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        repo.record_evidence(7, "temp/evidence/7.json", 1500)?; // replaces, newest now

        let rows = repo.list_evidence()?;
        assert_eq!(rows.iter().map(|r| r.job_id).collect::<Vec<_>>(), vec![3, 7]);
        assert_eq!(rows[1].bytes, 1500);
        assert!(rows.iter().all(|r| r.created_at.is_some()));

        repo.delete_evidence(3)?;
        assert_eq!(repo.list_evidence()?.len(), 1);
        repo.delete_evidence(3)?; // already gone: not an error
        Ok(())
    }

    #[test]
    fn site_stats_record_list_set_and_purge() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        assert!(repo.get_site("news.example")?.is_none());
        assert!(repo.list_sites_not_healthy()?.is_empty());

        for _ in 0..6 {
            let (b, a) = repo.record_site_outcome("news.example", SiteOutcome::Success)?;
            assert_eq!((b, a), (SiteState::Healthy, SiteState::Healthy));
        }
        let fail = || SiteOutcome::SiteFailure("NO_STREAM_FOUND".into());
        repo.record_site_outcome("news.example", fail())?;
        repo.record_site_outcome("news.example", fail())?;
        let (b, a) = repo.record_site_outcome("news.example", fail())?;
        assert_eq!((b, a), (SiteState::Healthy, SiteState::Degraded));

        let row = repo.get_site("news.example")?.expect("row");
        assert_eq!(row.consecutive_fail, 3);
        assert_eq!(row.recent, "111111000");
        assert_eq!(row.last_code.as_deref(), Some("NO_STREAM_FOUND"));
        assert!(row.last_ok_at.is_some() && row.last_fail_at.is_some() && row.state_since.is_some());
        let bad = repo.list_sites_not_healthy()?;
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].domain, "news.example");

        // A canary can call it broken; a failure leaves it broken.
        assert!(repo.set_site_state("news.example", SiteState::Broken)?);
        assert!(!repo.set_site_state("nobody.example", SiteState::Broken)?);
        let (b, a) = repo.record_site_outcome("news.example", fail())?;
        assert_eq!((b, a), (SiteState::Broken, SiteState::Broken));
        let (_, a) = repo.record_site_outcome("news.example", SiteOutcome::Success)?;
        assert_eq!(a, SiteState::Healthy);
        assert!(repo.list_sites_not_healthy()?.is_empty());

        // Purge: a recently active row stays, an idle one goes.
        repo.record_site_outcome("idle.example", SiteOutcome::Success)?;
        {
            let conn = repo.pool.get()?;
            let old = timestamps::format(Utc::now() - Duration::days(120));
            conn.execute(
                "UPDATE site_stats SET last_ok_at = ?, last_fail_at = NULL WHERE domain = 'idle.example'",
                params![old],
            )?;
        }
        assert_eq!(repo.purge_idle_sites(90)?, 1);
        assert!(repo.get_site("idle.example")?.is_none());
        assert!(repo.get_site("news.example")?.is_some());
        Ok(())
    }

    #[test]
    fn canary_pool_sources_and_crud() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let mk = |url: &str, slug: &str, status: &str, parent: Option<i64>| -> Result<i64> {
            let id = repo.enqueue(&NewJob::new(url, slug, "ANNA"), 0)?.job_id();
            let conn = repo.pool.get()?;
            conn.execute(
                "UPDATE queue SET status = ?, parent_job_id = ?, completed_at = ? WHERE id = ?",
                params![status, parent, timestamps::now_string(), id],
            )?;
            Ok(id)
        };
        let good = mk("https://news.example/a", "1_A_X", "COMPLETED", None)?;
        mk("https://news.example/b", "2_A_X", "COMPLETED_MANUAL", None)?;
        mk("https://news.example/c", "3_A_X", "COMPLETED", Some(good))?;
        mk("C:\\attachments\\clip.mp4", "4_A_X", "COMPLETED", None)?;
        mk("https://news.example/e", "5_A_X", "REQUIRES_REVIEW", None)?;
        let old = mk("https://news.example/old", "6_A_X", "COMPLETED", None)?;
        {
            let conn = repo.pool.get()?;
            conn.execute(
                "UPDATE queue SET completed_at = ? WHERE id = ?",
                params![timestamps::format(Utc::now() - Duration::days(40)), old],
            )?;
        }
        let pages = repo.completed_pages_since(30)?;
        assert_eq!(pages.len(), 1, "{pages:?}");
        assert_eq!((pages[0].job_id, pages[0].url.as_str()), (good, "https://news.example/a"));

        let t0 = Utc::now() - Duration::hours(5);
        assert!(repo.add_canary("News.Example", "https://news.example/a", Some(good), t0)?);
        assert!(!repo.add_canary("news.example", "https://news.example/a", Some(good), t0)?, "a link is added once");
        repo.add_canary("news.example", "https://news.example/z", None, t0 + Duration::hours(1))?;
        repo.add_canary("other.example", "https://other.example/1", None, t0)?;
        assert_eq!(repo.canary_domains()?, vec!["news.example", "other.example"]);
        let mine = repo.list_canaries_for("news.example")?;
        assert_eq!(mine.iter().map(|c| c.url.as_str()).collect::<Vec<_>>(), vec!["https://news.example/z", "https://news.example/a"]);
        assert!(mine[0].last_ok.is_none() && mine[0].last_checked_at.is_none());

        repo.record_canary_result(mine[0].id, false, "no video")?;
        repo.record_canary_result(mine[0].id, false, "no video")?;
        let c = &repo.list_canaries_for("news.example")?[0];
        assert_eq!((c.last_ok, c.fail_count, c.last_detail.as_deref()), (Some(false), 2, Some("no video")));
        assert!(c.last_checked_at.is_some());
        repo.record_canary_result(mine[0].id, true, "found it")?;
        let c = &repo.list_canaries_for("news.example")?[0];
        assert_eq!((c.last_ok, c.fail_count), (Some(true), 0));

        repo.delete_canary(mine[0].id)?;
        assert_eq!(repo.list_canaries()?.len(), 2);

        // A pass also clears the site's run of failures.
        repo.record_site_outcome("news.example", SiteOutcome::SiteFailure("NO_STREAM_FOUND".into()))?;
        assert_eq!(repo.get_site("news.example")?.unwrap().consecutive_fail, 1);
        repo.reset_site_failures("news.example")?;
        assert_eq!(repo.get_site("news.example")?.unwrap().consecutive_fail, 0);
        Ok(())
    }

    #[test]
    fn owns_lease_is_true_only_for_the_running_owner() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let id = repo.enqueue(&NewJob::new("https://example.gr/v", "1_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        assert!(!repo.owns_lease(id, "A")?, "a pending job is nobody's");
        repo.lease_job("A", 180)?.expect("leasable");
        assert!(repo.owns_lease(id, "A")?);
        assert!(!repo.owns_lease(id, "B")?);
        repo.delete_job(id)?;
        assert!(!repo.owns_lease(id, "A")?, "a deleted row is nobody's");
        Ok(())
    }

    #[test]
    fn pending_positions_follow_lease_order_and_skip_backoff() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let mut ids = Vec::new();
        for n in 0..4 {
            let job = NewJob::new(format!("https://example.gr/p{n}"), format!("{n}_ANNA_TEST"), "ANNA");
            ids.push(repo.enqueue(&job, DEFAULT_DEDUP_WINDOW_HOURS)?.job_id());
        }
        {
            let conn = repo.pool.get()?;
            // ids[0] oldest, ids[1] newer, ids[2] high priority but newest, ids[3] in back-off.
            for (i, id) in ids.iter().enumerate() {
                conn.execute(
                    "UPDATE queue SET created_at = ? WHERE id = ?",
                    params![format!("2030-01-01T00:00:0{i}.000Z"), id],
                )?;
            }
            conn.execute("UPDATE queue SET priority = 9 WHERE id = ?", params![ids[2]])?;
            conn.execute(
                "UPDATE queue SET not_before = '2999-01-01T00:00:00.000Z' WHERE id = ?",
                params![ids[3]],
            )?;
        }
        let pos = repo.pending_positions()?;
        assert_eq!(pos.get(&ids[2]), Some(&1), "priority first");
        assert_eq!(pos.get(&ids[0]), Some(&2));
        assert_eq!(pos.get(&ids[1]), Some(&3));
        assert_eq!(pos.get(&ids[3]), None, "back-off has no place");
        Ok(())
    }

    #[test]
    fn renaming_a_waiting_job_does_not_restart_its_wait() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let id = repo.enqueue(&NewJob::new("https://example.gr/r", "1_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        {
            let conn = repo.pool.get()?;
            conn.execute("UPDATE queue SET updated_at = '2030-01-01T00:00:00.000Z' WHERE id = ?", params![id])?;
        }
        assert!(repo.rename_job_keyword(id, "NEWWORD")?.is_some());
        let job = repo.get_job(id)?.unwrap();
        assert_eq!(job.keyword, "NEWWORD");
        assert_eq!(timestamps::format(job.updated_at.unwrap()), "2030-01-01T00:00:00.000Z");
        Ok(())
    }

    #[test]
    fn queue_watch_reports_oldest_wait_and_longest_step() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let now = Utc::now();
        assert_eq!(repo.queue_watch(now)?, QueueWatch::default());

        let a = repo.enqueue(&NewJob::new("https://example.gr/a", "1_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        let b = repo.enqueue(&NewJob::new("https://example.gr/b", "2_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        let c = repo.enqueue(&NewJob::new("https://example.gr/c", "3_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        let d = repo.enqueue(&NewJob::new("https://example.gr/d", "4_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        let leased = repo.lease_job("A", 180)?.expect("leasable");
        {
            let conn = repo.pool.get()?;
            let ago = |m: i64| timestamps::format(now - Duration::minutes(m));
            // Leased job is the one with the highest priority, oldest created_at: a.
            assert_eq!(leased.id, a);
            conn.execute("UPDATE queue SET stage = 'DOWNLOAD', stage_started_at = ? WHERE id = ?", params![ago(35), a])?;
            conn.execute("UPDATE queue SET updated_at = ? WHERE id = ?", params![ago(42), b])?;
            conn.execute("UPDATE queue SET updated_at = ? WHERE id = ?", params![ago(10), c])?;
            // In back-off and very old: must not count as waiting.
            conn.execute(
                "UPDATE queue SET updated_at = ?, not_before = '2999-01-01T00:00:00.000Z' WHERE id = ?",
                params![ago(500), d],
            )?;
        }
        let w = repo.queue_watch(now)?;
        assert_eq!(w.waiting, 2);
        assert_eq!(w.running, 1);
        assert_eq!(w.oldest_ready_wait_secs, Some(42 * 60));
        assert_eq!(w.longest_step, Some((a, "DOWNLOAD".to_string(), 35 * 60)));
        Ok(())
    }

    #[test]
    fn a_retried_job_starts_a_clean_run() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let id = repo.enqueue(&NewJob::new("https://example.gr/v", "1_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        repo.lease_job("A", 180)?.expect("leasable");
        assert!(repo.finish(id, "A", JobStatus::RequiresReview, Some("E_TEST"), Some("boom"), None)?);
        {
            let conn = repo.pool.get()?;
            conn.execute(
                "UPDATE queue SET attempts = max_attempts, not_before = '2999-01-01T00:00:00.000Z' WHERE id = ?",
                params![id],
            )?;
        }
        let before = repo.get_job(id)?.unwrap();
        assert!(before.attempts > 0 && before.error_code.is_some() && before.not_before.is_some());

        assert!(repo.retry_job(id, None)?);
        let job = repo.get_job(id)?.unwrap();
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.stage, JobStage::Queued);
        assert_eq!(job.attempts, 0);
        assert_eq!(job.error_code, None);
        assert_eq!(job.not_before, None);
        assert_eq!(job.lease_owner, None);
        assert_eq!(job.lease_expires_at, None);
        assert_eq!(job.stage_started_at, None);
        assert_eq!(job.completed_at, None);
        assert!(repo.lease_job("B", 180)?.is_some(), "a cleared not_before lets it lease at once");
        Ok(())
    }

    #[test]
    fn retry_is_allowed_exactly_for_the_waiting_and_stopped_statuses() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let cases = [
            (JobStatus::Pending, true),
            (JobStatus::RequiresReview, true),
            (JobStatus::ManualDownload, true),
            (JobStatus::Failed, true),
            (JobStatus::Cancelled, true),
            (JobStatus::Completed, false),
            (JobStatus::CompletedManual, false),
        ];
        for (n, (status, allowed)) in cases.into_iter().enumerate() {
            let url = format!("https://example.gr/v{n}");
            let id = repo.enqueue(&NewJob::new(&url, &format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
            repo.update_job_status(id, status, None, None, None)?;
            assert_eq!(repo.retry_job(id, None)?, allowed, "{}", status.as_str());
            let expected = if allowed { JobStatus::Pending } else { status };
            assert_eq!(repo.get_job(id)?.unwrap().status, expected, "{}", status.as_str());
        }
        Ok(())
    }

    #[test]
    fn the_purge_removes_videos_marked_done_by_hand_too() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let id = repo.enqueue(&NewJob::new("https://example.gr/old", "1_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        repo.update_job_status(id, JobStatus::RequiresReview, None, None, None)?;
        assert!(repo.mark_completed_manually(id)?);
        repo.pool.get()?.execute(
            "UPDATE queue SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?",
            params![id],
        )?;
        assert_eq!(repo.purge_completed_jobs(30)?, 1);
        assert!(repo.get_job(id)?.is_none());
        Ok(())
    }

    #[test]
    fn an_override_moves_the_dedup_key_with_the_link() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let id = repo.enqueue(&NewJob::new("https://example.gr/dead", "1_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        repo.update_job_status(id, JobStatus::RequiresReview, None, None, None)?;
        assert!(repo.retry_job(id, Some("https://example.gr/works"))?);
        let job = repo.get_job(id)?.unwrap();
        assert_eq!(job.url, "https://example.gr/works");
        assert_eq!(job.url_normalized.as_deref(), Some(crate::urlnorm::normalize("https://example.gr/works").as_str()));
        // A plain retry keeps the link and its key.
        repo.update_job_status(id, JobStatus::Failed, None, None, None)?;
        assert!(repo.retry_job(id, None)?);
        assert_eq!(repo.get_job(id)?.unwrap().url_normalized, job.url_normalized);
        Ok(())
    }

    #[test]
    fn a_running_or_delivered_job_cannot_be_retried() -> Result<()> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let id = repo.enqueue(&NewJob::new("https://example.gr/v", "1_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.job_id();
        repo.lease_job("A", 180)?.expect("leasable");
        assert!(!repo.retry_job(id, Some("https://example.gr/other"))?);
        let job = repo.get_job(id)?.unwrap();
        assert_eq!(job.status, JobStatus::Running);
        assert_eq!(job.lease_owner.as_deref(), Some("A"));
        assert_eq!(job.url, "https://example.gr/v");

        assert!(repo.finish(id, "A", JobStatus::Completed, None, None, Some("x.mxf"))?);
        assert!(!repo.retry_job(id, None)?);
        assert_eq!(repo.get_job(id)?.unwrap().status, JobStatus::Completed);
        Ok(())
    }

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

    /// A leased job whose stage clock is set by hand: `(repo, id, owner)`.
    fn leased_at(stage: &str, started: Option<chrono::DateTime<Utc>>) -> Result<(Repository, i64, &'static str, NamedTempFile)> {
        let temp_db = NamedTempFile::new()?;
        let repo = Repository::new(temp_db.path())?;
        let id = repo.add_job(
            "https://example.org/clip", "1_TESTER_CLIP", "TESTER", "CLIP", "1", 10,
            JobStatus::Pending, None, None, None,
        )?;
        let owner = "test-host:1:0";
        repo.lease_job(owner, 120)?.expect("leased");
        repo.pool.get()?.execute(
            "UPDATE queue SET stage = ?, stage_started_at = ? WHERE id = ?",
            params![stage, started.map(timestamps::format), id],
        )?;
        Ok((repo, id, owner, temp_db))
    }

    fn timings_of(repo: &Repository, id: i64) -> Option<serde_json::Value> {
        repo.get_job(id).unwrap().unwrap().stage_timings_json.map(|s| serde_json::from_str(&s).unwrap())
    }

    fn t0() -> chrono::DateTime<Utc> {
        timestamps::parse("2026-01-01T10:00:00.000Z").unwrap()
    }

    #[test]
    fn stage_timings_accumulate_per_stage_with_exact_clock() -> Result<()> {
        let (repo, id, owner, _db) = leased_at("DOWNLOAD", Some(t0()))?;
        // DOWNLOAD ran 1.5 s, then PROBE.
        assert!(repo.set_stage_at(id, owner, JobStage::Probe, t0() + Duration::milliseconds(1500))?);
        assert_eq!(timings_of(&repo, id), Some(serde_json::json!({"DOWNLOAD": 1500})));
        // PROBE 500 ms, back to DOWNLOAD (a retry inside the job), 2 s there.
        assert!(repo.set_stage_at(id, owner, JobStage::Download, t0() + Duration::milliseconds(2000))?);
        assert!(repo.set_stage_at(id, owner, JobStage::Probe, t0() + Duration::milliseconds(4000))?);
        assert_eq!(
            timings_of(&repo, id),
            Some(serde_json::json!({"DOWNLOAD": 3500, "PROBE": 500}))
        );
        Ok(())
    }

    #[test]
    fn refused_set_stage_writes_nothing_and_right_owner_does() -> Result<()> {
        let (repo, id, owner, _db) = leased_at("DOWNLOAD", Some(t0()))?;
        let later = t0() + Duration::seconds(9);
        assert!(!repo.set_stage_at(id, "someone-else", JobStage::Probe, later)?);
        let job = repo.get_job(id)?.unwrap();
        assert_eq!(job.stage, JobStage::Download);
        assert_eq!(job.stage_timings_json, None, "a refused move records no time");
        assert_eq!(job.stage_started_at, Some(t0()), "and does not restart the clock");
        // Negative control: the lease holder does write.
        assert!(repo.set_stage_at(id, owner, JobStage::Probe, later)?);
        assert_eq!(timings_of(&repo, id), Some(serde_json::json!({"DOWNLOAD": 9000})));
        Ok(())
    }

    #[test]
    fn malformed_timings_restart_and_the_stage_change_still_succeeds() -> Result<()> {
        let (repo, id, owner, _db) = leased_at("DOWNLOAD", Some(t0()))?;
        repo.pool.get()?.execute("UPDATE queue SET stage_timings_json = '{not json' WHERE id = ?", params![id])?;
        assert!(repo.set_stage_at(id, owner, JobStage::Probe, t0() + Duration::seconds(2))?);
        assert_eq!(repo.get_job(id)?.unwrap().stage, JobStage::Probe);
        assert_eq!(timings_of(&repo, id), Some(serde_json::json!({"DOWNLOAD": 2000})));
        Ok(())
    }

    #[test]
    fn null_stage_start_adds_no_key_and_negative_elapsed_is_zero() -> Result<()> {
        let (repo, id, owner, _db) = leased_at("DOWNLOAD", None)?;
        assert!(repo.set_stage_at(id, owner, JobStage::Probe, t0())?);
        assert_eq!(repo.get_job(id)?.unwrap().stage, JobStage::Probe);
        assert_eq!(timings_of(&repo, id), None);
        // The clock stepped back: PROBE started at t0, "now" is earlier.
        assert!(repo.set_stage_at(id, owner, JobStage::Transcode, t0() - Duration::seconds(30))?);
        assert_eq!(timings_of(&repo, id), Some(serde_json::json!({"PROBE": 0})));
        Ok(())
    }

    #[test]
    fn finish_adds_the_last_stage_and_a_refused_finish_writes_nothing() -> Result<()> {
        let (repo, id, owner, _db) = leased_at("TRANSCODE", Some(t0()))?;
        let end = t0() + Duration::milliseconds(7250);
        assert!(!repo.finish_at(id, "someone-else", JobStatus::Failed, None, None, None, end)?);
        let job = repo.get_job(id)?.unwrap();
        assert_eq!(job.status, JobStatus::Running);
        assert_eq!(job.stage_timings_json, None);
        assert!(repo.finish_at(id, owner, JobStatus::Completed, None, None, None, end)?);
        assert_eq!(timings_of(&repo, id), Some(serde_json::json!({"TRANSCODE": 7250})));
        Ok(())
    }

    #[test]
    fn repository_announces_stage_finish_and_throttled_progress() -> Result<()> {
        let (repo, id, owner, _db) = leased_at("DOWNLOAD", Some(t0()))?;
        let bus = crate::events::EventBus::new();
        let mut rx = bus.subscribe();
        let repo = repo.with_events(bus);
        let next = |rx: &mut tokio::sync::broadcast::Receiver<String>| -> serde_json::Value {
            serde_json::from_str(&rx.try_recv().expect("an event")).unwrap()
        };
        repo.set_stage(id, owner, JobStage::Probe)?;
        let v = next(&mut rx);
        assert_eq!((v["type"].as_str(), v["id"].as_i64(), v["stage"].as_str()), (Some("job"), Some(id), Some("PROBE")));
        assert_eq!(v["status"], "RUNNING");
        repo.update_job_progress(id, 10.0, "x", "--")?;
        repo.update_job_progress(id, 11.0, "x", "--")?;
        let v = next(&mut rx);
        assert_eq!(v["progress"], 10.0);
        assert_eq!(v["speed"], "x");
        assert_eq!(v["eta"], "--");
        assert_eq!(v["stage"], "PROBE");
        assert!(rx.try_recv().is_err(), "second tick inside a second is dropped");
        // A refused move announces nothing.
        assert!(!repo.set_stage(id, "someone-else", JobStage::Transcode)?);
        assert!(rx.try_recv().is_err());
        repo.finish(id, owner, JobStatus::Completed, None, None, None)?;
        let v = next(&mut rx);
        assert_eq!((v["status"].as_str(), v["stage"].as_str(), v["progress"].as_f64()), (Some("COMPLETED"), Some("DONE"), Some(100.0)));
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

/// The `held_url` of each of the job's attempts MCR rejected, oldest first.
fn rejected_urls_in(conn: &rusqlite::Connection, job_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT held_url FROM repair_attempts
         WHERE job_id = ? AND mcr_decision = 'rejected' AND held_url IS NOT NULL ORDER BY id",
    )?;
    let rows = stmt.query_map(params![job_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// What a decision teaches the recipe store (P10.13).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Learn {
    Nothing,
    /// MCR approved the held video: the recipe behind it gains an approval.
    Approved,
    /// MCR said it is the wrong video: the recipe is rejected on that site and
    /// the address is skipped for 30 days.
    WrongVideo,
}

/// How long «Λάθος βίντεο» keeps an address out of the ladder, and how many a domain keeps.
const SKIP_DAYS: i64 = 30;
const SKIPS_PER_DOMAIN: i64 = 50;

/// The repair attempt a decision is about.
struct OpenHold {
    id: i64,
    recipe_id: Option<i64>,
    recipe_hash: Option<String>,
    domain: Option<String>,
    held_url: Option<String>,
    candidates_json: Option<String>,
    next_index: i64,
}

/// The job's latest repair attempt that held a video nobody decided on yet.
fn open_hold(tx: &rusqlite::Transaction<'_>, job_id: i64) -> Result<Option<OpenHold>> {
    Ok(tx
        .query_row(
            "SELECT id, recipe_id, recipe_hash, domain, held_url, candidates_json, next_index FROM repair_attempts
             WHERE job_id = ? AND outcome = 'held' AND mcr_decision IS NULL ORDER BY id DESC LIMIT 1",
            params![job_id],
            |r| {
                Ok(OpenHold {
                    id: r.get(0)?,
                    recipe_id: r.get(1)?,
                    recipe_hash: r.get(2)?,
                    domain: r.get(3)?,
                    held_url: r.get(4)?,
                    candidates_json: r.get(5)?,
                    next_index: r.get(6)?,
                })
            },
        )
        .optional()?)
}

/// MCR decided about a held video: note it on the job's latest repair attempt
/// that held one and has no decision yet, and teach the recipe store what the
/// decision says (P10.13), in the same transaction as the decision itself, so
/// a failure of either rolls both back. Nothing to note for a hold the ladder
/// did not make. Returns that attempt.
fn note_mcr_decision(
    tx: &rusqlite::Transaction<'_>,
    job_id: i64,
    decision: &str,
    reason: Option<&str>,
    learn: Learn,
    now: chrono::DateTime<Utc>,
) -> Result<Option<OpenHold>> {
    let Some(hold) = open_hold(tx, job_id)? else { return Ok(None) };
    tx.execute(
        "UPDATE repair_attempts SET mcr_decision = ?, mcr_reason = ? WHERE id = ?",
        params![decision, reason, hold.id],
    )?;
    if learn == Learn::Nothing {
        return Ok(Some(hold));
    }
    if let (Some(recipe_id), Some(domain)) = (hold.recipe_id, hold.domain.as_deref()) {
        recipe_sql::learn_from_mcr(tx, recipe_id, hold.recipe_hash.as_deref(), domain, learn == Learn::Approved, now)?;
    }
    if learn == Learn::WrongVideo {
        if let (Some(domain), Some(pattern)) = (hold.domain.as_deref(), hold.held_url.as_deref().and_then(crate::urlnorm::skip_pattern)) {
            write_skip(tx, domain, &pattern, now)?;
        }
    }
    Ok(Some(hold))
}

/// Remember `pattern` as a wrong video on `domain` for 30 days (a second
/// rejection extends it); the domain keeps at most 50, the soonest to expire out.
fn write_skip(tx: &rusqlite::Transaction<'_>, domain: &str, pattern: &str, now: chrono::DateTime<Utc>) -> Result<()> {
    let until = timestamps::format(now + Duration::days(SKIP_DAYS));
    tx.execute(
        "INSERT INTO repair_skips (domain, pattern, until) VALUES (?, ?, ?)
         ON CONFLICT(domain, pattern) DO UPDATE SET until = MAX(until, excluded.until)",
        params![domain, pattern, until],
    )?;
    tx.execute(
        "DELETE FROM repair_skips WHERE domain = ?1 AND pattern NOT IN
           (SELECT pattern FROM repair_skips WHERE domain = ?1 ORDER BY until DESC, rowid DESC LIMIT ?2)",
        params![domain, SKIPS_PER_DOMAIN],
    )?;
    Ok(())
}

/// The still-valid skip patterns of `domain`.
fn skips_of(tx: &rusqlite::Connection, domain: &str, now: chrono::DateTime<Utc>) -> Result<Vec<String>> {
    let mut stmt = tx.prepare("SELECT pattern FROM repair_skips WHERE domain = ? AND until > ? ORDER BY pattern")?;
    let rows = stmt.query_map(params![domain, timestamps::format(now)], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The places in `candidates_json` after `next_index` that may still be
/// offered: not an address MCR rejected or the one held, and not skipped.
fn next_options(candidates_json: Option<&str>, next_index: i64, excluded: &[String], skips: &[String]) -> Vec<usize> {
    let list: Vec<serde_json::Value> = candidates_json.and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
    let barred: std::collections::HashSet<String> = excluded.iter().map(|u| crate::urlnorm::normalize(u)).collect();
    // The held and the rejected addresses are skipped by pattern too: a copy that differs
    // only in its query is the same video, and the decision's own skip would bar it.
    let barred_patterns: std::collections::HashSet<String> = excluded.iter().filter_map(|u| crate::urlnorm::skip_pattern(u)).collect();
    let first = usize::try_from(next_index).map_or(0, |i| i + 1);
    list.iter()
        .enumerate()
        .skip(first)
        .filter(|(_, c)| {
            let Some(url) = c.get("url").and_then(serde_json::Value::as_str) else { return false };
            !barred.contains(&crate::urlnorm::normalize(url))
                && !crate::urlnorm::skip_pattern(url).is_some_and(|p| skips.contains(&p) || barred_patterns.contains(&p))
        })
        .map(|(i, _)| i)
        .collect()
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

fn site_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SiteRow> {
    let ts = |i: usize| -> rusqlite::Result<Option<chrono::DateTime<Utc>>> {
        Ok(timestamps::parse_opt(r.get::<_, Option<String>>(i)?))
    };
    Ok(SiteRow {
        domain: r.get(0)?,
        recent: r.get(1)?,
        consecutive_fail: r.get(2)?,
        last_ok_at: ts(3)?,
        last_fail_at: ts(4)?,
        last_code: r.get(5)?,
        state: SiteState::parse(&r.get::<_, String>(6)?),
        state_since: ts(7)?,
        llm_breaker_until: ts(8)?,
    })
}

fn read_canaries(conn: &rusqlite::Connection, domain: Option<&str>) -> Result<Vec<crate::models::SiteCanary>> {
    let mut stmt = conn.prepare(
        "SELECT id, domain, url, job_id, added_at, last_checked_at, last_ok, last_detail, fail_count
         FROM site_canaries WHERE (?1 IS NULL OR domain = ?1) ORDER BY added_at DESC, id DESC",
    )?;
    let rows = stmt
        .query_map(params![domain], |r| {
            Ok(crate::models::SiteCanary {
                id: r.get(0)?,
                domain: r.get(1)?,
                url: r.get(2)?,
                job_id: r.get(3)?,
                added_at: timestamps::parse_opt(r.get(4)?),
                last_checked_at: timestamps::parse_opt(r.get(5)?),
                last_ok: r.get::<_, Option<i64>>(6)?.map(|v| v != 0),
                last_detail: r.get(7)?,
                fail_count: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn read_site(conn: &rusqlite::Connection, domain: &str) -> Result<Option<SiteRow>> {
    Ok(conn
        .query_row(
            "SELECT domain, recent, consecutive_fail, last_ok_at, last_fail_at, last_code,
                    state, state_since, llm_breaker_until
             FROM site_stats WHERE domain = ?",
            params![domain],
            site_from_row,
        )
        .optional()?)
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

#[cfg(test)]
mod own_job_tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn job_for(repo: &Repository, user: i64, tag: &str) -> Result<i64> {
        repo.add_job(
            &format!("https://example.test/video/{tag}"),
            &format!("1_TESTER_{tag}"),
            "TESTER",
            tag,
            "1",
            0,
            JobStatus::Pending,
            Some(user),
            None,
            None,
        )
    }

    fn status_of(repo: &Repository, id: i64) -> String {
        let conn = repo.pool.get().unwrap();
        conn.query_row("SELECT status FROM queue WHERE id = ?", params![id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn cancel_own_pending_refuses_another_users_job_and_leaves_it_pending() -> Result<()> {
        let db = NamedTempFile::new()?;
        let repo = Repository::new(db.path())?;
        let a = repo.create_user("a@example.test", "pw", UserRole::User, "A Tester", Some("TESTERA"))?;
        let b = repo.create_user("b@example.test", "pw", UserRole::User, "B Tester", Some("TESTERB"))?;
        let job = job_for(&repo, a, "ONE")?;

        assert_eq!(repo.cancel_own_pending(job, b)?, OwnCancelOutcome::NotFound);
        assert_eq!(status_of(&repo, job), "PENDING");
        assert_eq!(repo.cancel_own_pending(9999, a)?, OwnCancelOutcome::NotFound);
        assert_eq!(repo.cancel_own_pending(job, a)?, OwnCancelOutcome::Cancelled);
        assert_eq!(status_of(&repo, job), "CANCELLED");
        // Already cancelled: own job, no longer waiting.
        assert_eq!(repo.cancel_own_pending(job, a)?, OwnCancelOutcome::NotCancellable);
        Ok(())
    }

    #[test]
    fn cancel_own_pending_refuses_running_and_completed() -> Result<()> {
        let db = NamedTempFile::new()?;
        let repo = Repository::new(db.path())?;
        let a = repo.create_user("a@example.test", "pw", UserRole::User, "A Tester", Some("TESTERA"))?;
        for (tag, status) in [("RUN", "RUNNING"), ("DONE", "COMPLETED")] {
            let job = job_for(&repo, a, tag)?;
            repo.pool.get()?.execute("UPDATE queue SET status = ? WHERE id = ?", params![status, job])?;
            assert_eq!(repo.cancel_own_pending(job, a)?, OwnCancelOutcome::NotCancellable);
            assert_eq!(status_of(&repo, job), status);
        }
        Ok(())
    }

    #[test]
    fn notify_preference_is_per_user_and_defaults_off() -> Result<()> {
        let db = NamedTempFile::new()?;
        let repo = Repository::new(db.path())?;
        let a = repo.create_user("a@example.test", "pw", UserRole::User, "A Tester", None)?;
        let b = repo.create_user("b@example.test", "pw", UserRole::User, "B Tester", None)?;
        assert_eq!(repo.get_notify_on_completion(a)?, Some(false));
        assert!(repo.set_notify_on_completion(a, true)?);
        assert_eq!(repo.get_notify_on_completion(a)?, Some(true));
        assert_eq!(repo.get_notify_on_completion(b)?, Some(false));
        assert_eq!(repo.get_notify_on_completion(424242)?, None);
        Ok(())
    }

    #[test]
    fn notify_column_is_added_with_default_zero_to_existing_users() -> Result<()> {
        // A database that stopped just before the notify migration, with a user already in it.
        let mut conn = rusqlite::Connection::open_in_memory()?;
        migrations::current_version(&conn)?;
        let notify_v = migrations::MIGRATIONS.iter().find(|(_, sql)| sql.contains("notify_on_completion")).map(|(v, _)| *v).expect("the notify migration exists");
        for (v, sql) in migrations::MIGRATIONS.iter().filter(|(v, _)| *v < notify_v) {
            conn.execute_batch(sql)?;
            conn.execute(
                "INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (?, 'x')",
                params![v],
            )?;
        }
        conn.execute(
            "INSERT INTO users (email, password_hash, role, full_name, is_active) VALUES ('old@example.test','h','user','Old',1)",
            [],
        )?;
        migrations::apply(&mut conn, None)?;
        let n: i64 = conn.query_row(
            "SELECT notify_on_completion FROM users WHERE email='old@example.test'", [], |r| r.get(0))?;
        assert_eq!(n, 0);
        assert_eq!(migrations::current_version(&conn)?, migrations::target_version());
        Ok(())
    }
}
