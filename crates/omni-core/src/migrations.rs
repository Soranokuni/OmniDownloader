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
    (
        3,
        // Session hardening (plan P2.2). Sessions carried only a token, a user
        // and an expiry, so an admin could neither see nor end the sessions on
        // their account, and there was no idle timeout -- an unattended MCR
        // browser stayed authenticated for the full absolute lifetime.
        //
        // Existing rows are backfilled with 'now' rather than dropped: a
        // migration that logs the whole newsroom out mid-bulletin is a worse
        // failure than one stale `last_seen_at`.
        r#"
        ALTER TABLE sessions ADD COLUMN created_at   TEXT NOT NULL DEFAULT '';
        ALTER TABLE sessions ADD COLUMN last_seen_at TEXT NOT NULL DEFAULT '';
        ALTER TABLE sessions ADD COLUMN ip           TEXT;
        ALTER TABLE sessions ADD COLUMN user_agent   TEXT;

        UPDATE sessions
           SET created_at   = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
               last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE created_at = '';

        CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);

        -- Login attempts are audited so a lockout can be explained after the
        -- fact ("who was hitting it, from where"). Kept out of audit_log
        -- because that table is what the panel shows operators, and a brute
        -- force would bury every operational message in it.
        CREATE TABLE IF NOT EXISTS login_attempts (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            at         TEXT NOT NULL,
            email      TEXT,
            ip         TEXT,
            successful INTEGER NOT NULL DEFAULT 0,
            reason     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_login_attempts_at ON login_attempts(at);
        "#,
    ),
    (
        4,
        // Maintenance scheduling (plan P6.6).
        //
        // The previous scheduler was `if now.format("%H") == "03"` inside an
        // hourly tick. Three problems it could not express: a task that should
        // run at 03:30 rather than on the hour; a task that was missed because
        // the machine was off at 03:00 and should run at next start; and any
        // record at all of whether last night's run worked. Persisting
        // `next_run` answers all three.
        r#"
        CREATE TABLE IF NOT EXISTS scheduled_tasks (
            name         TEXT PRIMARY KEY,
            enabled      INTEGER NOT NULL DEFAULT 1,
            next_run     TEXT,
            last_run     TEXT,
            last_outcome TEXT,
            last_error   TEXT,
            last_ms      INTEGER
        );
        "#,
    ),
    (
        5,
        // Per-message idempotency for email ingest (plan P4.2, defect E-07).
        //
        // A server can present a message as unread again: an IMAP flag lost
        // on reconnect, a Graph move that failed after the jobs were queued,
        // a journalist dragging it back to the inbox. Keyed on the RFC 5322
        // Message-ID, which survives all three, so the second sighting is
        // recognised instead of re-parsed. `jobs_json` lists what the message
        // produced, for the summary reply (plan P5.2).
        r#"
        CREATE TABLE IF NOT EXISTS processed_mail (
            internet_message_id TEXT PRIMARY KEY,
            source_id    TEXT,
            processed_at TEXT NOT NULL,
            outcome      TEXT NOT NULL,
            from_address TEXT,
            subject      TEXT,
            jobs_json    TEXT NOT NULL DEFAULT '[]'
        );
        CREATE INDEX IF NOT EXISTS idx_processed_mail_at ON processed_mail(processed_at);
        "#,
    ),
    (
        6,
        // Where the last mail poll got to, per mailbox (plan P4.8).
        //
        // The station's Graph app has `Mail.Read` only, so nothing can be
        // marked read or moved: "unread" says nothing about what the daemon
        // has handled. The poll lists what changed since this point (minus an
        // overlap) and `processed_mail` decides what is new. Losing the row
        // costs Graph calls, never a duplicate job.
        r#"
        CREATE TABLE IF NOT EXISTS mail_checkpoints (
            mailbox        TEXT PRIMARY KEY,
            modified_since TEXT NOT NULL,
            updated_at     TEXT NOT NULL
        );
        "#,
    ),
    (
        7,
        // The newsroom taxonomy (plan P4.17): groups (the news desk, each
        // show) and who belongs to which. `position` orders a person's
        // groups; position 0 is their default. A group is a label on the job
        // and never changes a file name or a delivery path.
        r#"
        CREATE TABLE IF NOT EXISTS groups (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            code        TEXT NOT NULL UNIQUE,
            name        TEXT NOT NULL,
            kind        TEXT NOT NULL DEFAULT 'show',
            keywords    TEXT NOT NULL DEFAULT '[]',
            description TEXT NOT NULL DEFAULT '',
            created_at  TEXT
        );
        CREATE TABLE IF NOT EXISTS journalist_groups (
            journalist_id INTEGER NOT NULL REFERENCES journalists(id) ON DELETE CASCADE,
            group_id      INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
            position      INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (journalist_id, group_id)
        );
        "#,
    ),
    (
        8,
        // The group a job was queued for (plan P4.18): a label for the
        // panels, never used for naming or delivery. A job keeps its label
        // if the group is later deleted.
        r#"
        ALTER TABLE queue ADD COLUMN group_code TEXT;
        "#,
    ),
    (
        9,
        // Mail an operator asked to have read again (plan P4.25). The
        // watcher fetches each by its provider id on its next poll and runs
        // it through the parser as if new; a checkpoint rewind would not
        // reach a mail changed long ago. `attempts` counts failed tries.
        r#"
        CREATE TABLE IF NOT EXISTS mail_reprocess (
            internet_message_id TEXT PRIMARY KEY,
            source_id    TEXT NOT NULL,
            requested_at TEXT NOT NULL,
            requested_by TEXT,
            attempts     INTEGER NOT NULL DEFAULT 0
        );
        "#,
    ),
    (
        10,
        // The nightly self-check (plan P6.7): links known to hold a video,
        // one per route a job can take, checked without downloading. The
        // defaults are seeded here, once, so links an operator deletes stay
        // deleted. `last_ok` is NULL until the first check.
        r#"
        CREATE TABLE IF NOT EXISTS selfcheck_links (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            label           TEXT NOT NULL,
            url             TEXT NOT NULL UNIQUE,
            last_checked_at TEXT,
            last_ok         INTEGER,
            last_detail     TEXT,
            last_ok_at      TEXT,
            failing_since   TEXT,
            created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        );
        INSERT OR IGNORE INTO selfcheck_links (label, url) VALUES
            ('YouTube', 'https://www.youtube.com/watch?v=jNQXAC9IVRw'),
            ('Instagram reel', 'https://www.instagram.com/reel/Dd615LBN4QM/'),
            ('Facebook video', 'https://www.facebook.com/ingr.officialpage/videos/1245278808680006/'),
            ('TikTok', 'https://www.tiktok.com/@glasatnabulgaria/video/7690626350159318305'),
            ('X (Twitter) video', 'https://x.com/i/status/2100509173943288138'),
            ('Streamable', 'https://streamable.com/531cym'),
            ('News article, glomex player', 'https://www.neakriti.gr/life/2202679_katerina-liolioy-makari-na-me-axiosei-o-theos-na-kano-ena-i-dyo-paidia-ti-eipe-gia'),
            ('News article, Instagram embed', 'https://www.iefimerida.gr/zoi/keit-mintleton-doyleies-spitioy-prigkipa-goyiliam'),
            ('News article, YouTube embed', 'https://www.bovary.gr/people-and-style/glam-stars/tzoni-ntep-entyposiaki-metamorfosi-gkriza-mallia');
        "#,
    ),
    (
        11,
        // MCR desk paging (plan P7.1): "Clear finished" takes delivered jobs
        // off the live queue without deleting them; they stay under
        // Completed, where they can be downloaded again.
        r#"
        ALTER TABLE queue ADD COLUMN cleared_at TEXT;
        CREATE INDEX IF NOT EXISTS idx_queue_status_completed ON queue(status, completed_at);
        "#,
    ),
    (
        12,
        // "ΓΙΑ ΠΛΑΝΑ: 2 ΠΡΩΤΑ ΒΙΝΤΕΟ" (plan P4.33): how many of an article's
        // videos the journalist asked for. NULL means all of them.
        r#"
        ALTER TABLE queue ADD COLUMN max_videos INTEGER;
        "#,
    ),
    (
        13,
        // The MCR mail view (plan P7.6): what each mail said, so the desk can
        // show it next to the jobs it produced. `body_text` is the readable
        // text (never the HTML) and is cleared by the nightly retention task
        // after `mail_text_retention_days`; the rest stays with the row.
        //
        // `parent_job_id`: the job whose article an article sibling (1B, 1C)
        // or an offer MCR queued came from. Rows queued before this have it
        // only in their notes, in the two forms the planner and the offer
        // route write; CAST keeps the leading digits of "123: https://…".
        r#"
        ALTER TABLE processed_mail ADD COLUMN received_at      TEXT;
        ALTER TABLE processed_mail ADD COLUMN from_name        TEXT;
        ALTER TABLE processed_mail ADD COLUMN to_addrs         TEXT;
        ALTER TABLE processed_mail ADD COLUMN cc_addrs         TEXT;
        ALTER TABLE processed_mail ADD COLUMN body_text        TEXT;
        ALTER TABLE processed_mail ADD COLUMN attachments_json TEXT;
        ALTER TABLE processed_mail ADD COLUMN parse_json       TEXT;
        CREATE INDEX IF NOT EXISTS idx_processed_mail_received ON processed_mail(received_at);

        ALTER TABLE queue ADD COLUMN parent_job_id INTEGER;
        UPDATE queue
           SET parent_job_id = CAST(substr(notes, length('Also in the article of job #') + 1) AS INTEGER)
         WHERE notes LIKE 'Also in the article of job #%';
        UPDATE queue
           SET parent_job_id = CAST(substr(notes, length('Offered from the article of job #') + 1) AS INTEGER)
         WHERE notes LIKE 'Offered from the article of job #%';
        UPDATE queue SET parent_job_id = NULL WHERE parent_job_id <= 0;
        CREATE INDEX IF NOT EXISTS idx_queue_email_message ON queue(email_message_id);
        CREATE INDEX IF NOT EXISTS idx_queue_parent        ON queue(parent_job_id);
        "#,
    ),
    (
        14,
        // Self-check links for the routes added on 2026-10-07 (P3.8–P3.13):
        // a news agency page whose video only its own API names, an article
        // whose first embedded X post is photos only, and the same article
        // with the "-ΒΙΝΤΕΟ" a sender glued on (404 as sent). Seeded once,
        // like migration 10's: an operator's deletion sticks.
        r#"
        INSERT OR IGNORE INTO selfcheck_links (label, url) VALUES
            ('News agency video page (ΑΠΕ-ΜΠΕ API)', 'https://www.amna.gr/home/videos/1028434/Proores-ekloges-stin-Ispania-stis-29-Noembriou--anakoinose-o-P-Santseth'),
            ('News article, X embeds, first one photos only', 'https://www.news247.gr/kosmos/rosia-apo-agnosti-pnevmonia-o-thanatos-tis-erevnitrias-ti-leei-gia-tin-panoli-o-pou/'),
            ('Link with "-ΒΙΝΤΕΟ" glued on by the sender', 'https://www.news247.gr/kosmos/rosia-apo-agnosti-pnevmonia-o-thanatos-tis-erevnitrias-ti-leei-gia-tin-panoli-o-pou/-ΒΙΝΤΕΟ');
        "#,
    ),
    (
        15,
        // Page evidence of failed news-page jobs (P10.1): one capped JSON
        // file per job under temp/evidence; the row is what retention walks.
        r#"
        CREATE TABLE IF NOT EXISTS job_evidence (
            job_id     INTEGER PRIMARY KEY,
            path       TEXT NOT NULL,
            bytes      INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );
        "#,
    ),
    (
        16,
        // Per news site: has it started failing (P10.3)? `recent` is the last
        // 20 outcomes as '1'/'0'; `llm_breaker_until` is for a later item.
        r#"
        CREATE TABLE IF NOT EXISTS site_stats (
            domain            TEXT PRIMARY KEY,
            recent            TEXT NOT NULL DEFAULT '',
            consecutive_fail  INTEGER NOT NULL DEFAULT 0,
            last_ok_at        TEXT,
            last_fail_at      TEXT,
            last_code         TEXT,
            state             TEXT NOT NULL DEFAULT 'healthy',
            state_since       TEXT,
            llm_breaker_until TEXT,
            updated_at        TEXT NOT NULL
        );
        "#,
    ),
    (
        17,
        // Recent known-good article links per frequent news site, re-checked
        // each night and when a site starts failing (P10.4). Not
        // `selfcheck_links`: that list also decides whether a yt-dlp update
        // is kept. `added_at` is when the job that proved the link finished.
        r#"
        CREATE TABLE IF NOT EXISTS site_canaries (
            id              INTEGER PRIMARY KEY,
            domain          TEXT NOT NULL,
            url             TEXT NOT NULL UNIQUE,
            job_id          INTEGER,
            added_at        TEXT NOT NULL,
            last_checked_at TEXT,
            last_ok         INTEGER,
            last_detail     TEXT,
            fail_count      INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_site_canaries_domain ON site_canaries(domain);
        "#,
    ),
    (
        18,
        // A finished video held back from the watchfolder until MCR approves
        // it (P10.5), bound to its SHA-256. `decision`: NULL (waiting) |
        // approved | rejected | expired | superseded | delivered.
        r#"
        CREATE TABLE IF NOT EXISTS held_outputs (
            job_id     INTEGER PRIMARY KEY,
            path       TEXT NOT NULL,
            bytes      INTEGER NOT NULL,
            sha256     TEXT NOT NULL,
            duration   REAL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            decision   TEXT,
            decided_by TEXT,
            decided_at TEXT,
            reason     TEXT,
            delivered_path TEXT
        );
        "#,
    ),
    (
        19,
        // Learned page recipes (P10.10), bounded and scored per site. `hash`
        // is scope + recipe; `steps_hash` is the steps alone, so one set of
        // steps can be shared by every site that uses the same video player.
        // A tombstone keeps a recipe MCR rejected from coming back.
        r#"
        CREATE TABLE IF NOT EXISTS recipes (
            id             INTEGER PRIMARY KEY,
            hash           TEXT NOT NULL UNIQUE,
            steps_hash     TEXT NOT NULL,
            scope_kind     TEXT NOT NULL,
            scope_key      TEXT NOT NULL,
            family         TEXT,
            recipe_json    TEXT NOT NULL,
            dsl_version    INTEGER NOT NULL,
            origin         TEXT NOT NULL,
            state          TEXT NOT NULL,
            created_at     TEXT NOT NULL,
            retired_at     TEXT,
            retired_reason TEXT,
            revived_at     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_recipes_scope ON recipes(scope_kind, scope_key);
        CREATE INDEX IF NOT EXISTS idx_recipes_steps ON recipes(steps_hash);
        CREATE TABLE IF NOT EXISTS recipe_sites (
            recipe_id        INTEGER NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
            domain           TEXT NOT NULL,
            alpha            REAL NOT NULL DEFAULT 1,
            beta             REAL NOT NULL DEFAULT 1,
            scored_at        TEXT NOT NULL,
            created_at       TEXT NOT NULL,
            approvals        INTEGER NOT NULL DEFAULT 0,
            rejections       INTEGER NOT NULL DEFAULT 0,
            uses             INTEGER NOT NULL DEFAULT 0,
            consecutive_fail INTEGER NOT NULL DEFAULT 0,
            pass_streak      INTEGER NOT NULL DEFAULT 0,
            state            TEXT NOT NULL DEFAULT 'candidate',
            suspended_reason TEXT,
            suspended_at     TEXT,
            last_used_at     TEXT,
            last_ok_at       TEXT,
            PRIMARY KEY (recipe_id, domain)
        );
        CREATE INDEX IF NOT EXISTS idx_recipe_sites_domain ON recipe_sites(domain);
        CREATE TABLE IF NOT EXISTS recipe_tombstones (
            hash   TEXT PRIMARY KEY,
            reason TEXT NOT NULL,
            at     TEXT NOT NULL,
            steps_hash TEXT,
            scope_kind TEXT,
            scope_key  TEXT
        );
        CREATE TABLE IF NOT EXISTS recipe_bars (
            steps_hash TEXT NOT NULL,
            domain     TEXT NOT NULL,
            at         TEXT NOT NULL,
            PRIMARY KEY (steps_hash, domain)
        );
        "#,
    ),
    (
        20,
        // One row per run of the repair ladder on a job (P10.11a): what it
        // tried, the verified candidates in rank order (`candidates_json`,
        // at most 32 KiB), and the full hash of the recipe behind the top
        // candidate so MCR's decision can find it. `held_url` is the exact
        // address that was held, `next_index` its place in the list, and
        // `mcr_decision` (approved | rejected | expired) is what the desk
        // later decided about it. Goes with its job and after 90 days.
        r#"
        CREATE TABLE IF NOT EXISTS repair_attempts (
            id              INTEGER PRIMARY KEY,
            job_id          INTEGER NOT NULL REFERENCES queue(id) ON DELETE CASCADE,
            started_at      TEXT NOT NULL,
            finished_at     TEXT,
            mode            TEXT NOT NULL,
            rung            TEXT,
            outcome         TEXT,
            recipe_hash     TEXT,
            recipe_id       INTEGER,
            domain          TEXT,
            llm_calls       INTEGER NOT NULL DEFAULT 0,
            tokens_in       INTEGER NOT NULL DEFAULT 0,
            tokens_out      INTEGER NOT NULL DEFAULT 0,
            candidates_json TEXT,
            next_index      INTEGER NOT NULL DEFAULT 0,
            duration_ms     INTEGER,
            held_url        TEXT,
            mcr_decision    TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_repair_attempts_job ON repair_attempts(job_id);
        "#,
    ),
    (
        21,
        // What the approval card shows next to a held repaired video
        // (P10.11b): how far the article's picture is from the closest frame
        // of the preview (a dHash distance; NULL = no article picture or no
        // comparison), the time of that frame, the host the picture came from,
        // and the length the page itself stated for its video.
        r#"
        ALTER TABLE repair_attempts ADD COLUMN image_distance   INTEGER;
        ALTER TABLE repair_attempts ADD COLUMN image_frame_secs REAL;
        ALTER TABLE repair_attempts ADD COLUMN image_host       TEXT;
        ALTER TABLE repair_attempts ADD COLUMN expected_secs    REAL;
        "#,
    ),
    (
        22,
        // The LLM rung's limits (P10.12): calls and tokens per local day (the
        // daily cap lives here, so it holds across restarts and jobs), and the
        // page bundles sent in the last 24 h (the same bundle is not sent twice).
        r#"
        CREATE TABLE IF NOT EXISTS llm_usage (
            day        TEXT PRIMARY KEY,
            calls      INTEGER NOT NULL DEFAULT 0,
            tokens_in  INTEGER NOT NULL DEFAULT 0,
            tokens_out INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS repair_llm_sent (
            hash   TEXT PRIMARY KEY,
            domain TEXT NOT NULL,
            at     TEXT NOT NULL
        );
        "#,
    ),
    (
        23,
        // Videos MCR called «Λάθος βίντεο» (P10.13): per page domain, the
        // host and path (no scheme, query or fragment) of the address, kept
        // 30 days (`until`), at most 50 per domain. The repair ladder drops
        // any candidate with such a pattern before the gate.
        r#"
        CREATE TABLE IF NOT EXISTS repair_skips (
            domain  TEXT NOT NULL,
            pattern TEXT NOT NULL,
            until   TEXT NOT NULL,
            PRIMARY KEY (domain, pattern)
        );
        "#,
    ),
    (
        24,
        // What the overnight self-heal found per broken site (P10.14): one row
        // per run, a host and a recipe hash only (no page address). Kept 30
        // days, at most 500 rows; MCR's morning line and the admin card read it.
        r#"
        CREATE TABLE IF NOT EXISTS selfheal_reports (
            id          INTEGER PRIMARY KEY,
            domain      TEXT NOT NULL,
            at          TEXT NOT NULL,
            outcome     TEXT NOT NULL,
            recipe_hash TEXT,
            origin      TEXT,
            pages       INTEGER NOT NULL DEFAULT 0,
            detail      TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_selfheal_reports_at ON selfheal_reports(at);
        "#,
    ),
    (
        25,
        // Why MCR rejected a held video (P10.15a): `wrong_video` or another
        // reason, written with the decision, so the repair report counts each
        // rejection by its own reason. Rows from before have none (count as other).
        r#"
        ALTER TABLE repair_attempts ADD COLUMN mcr_reason TEXT;
        "#,
    ),
    (
        26,
        // Cookie jars for login-walled sites (P3.5). `sealed` is the Netscape text
        // under machine-scope DPAPI; every other column is safe to show. One jar per
        // registrable domain (x.com also serves twitter.com).
        r#"
        CREATE TABLE IF NOT EXISTS cookie_jars (
            domain           TEXT PRIMARY KEY,
            sealed           BLOB NOT NULL,
            cookie_count     INTEGER NOT NULL,
            expires_min      TEXT,
            updated_at       TEXT NOT NULL,
            updated_by       TEXT NOT NULL,
            last_test_at     TEXT,
            last_test_ok     INTEGER,
            last_test_detail TEXT,
            last_sample_url  TEXT
        );
        "#,
    ),
    (
        27,
        // The journalist's own "tell me when it is delivered" choice (P7.3). Only
        // stored here; sending is a later item.
        r#"
        ALTER TABLE users ADD COLUMN notify_on_completion INTEGER NOT NULL DEFAULT 0;
        "#,
    ),
    (
        28,
        // The end-to-end benchmark (P6.4): the stored 65-link corpus, one row per run and one
        // per target of a run. A result keeps the host of the stream it found, never the address.
        r#"
        CREATE TABLE IF NOT EXISTS benchmark_targets (
            id       INTEGER PRIMARY KEY,
            idx      INTEGER NOT NULL UNIQUE,
            url      TEXT NOT NULL,
            domain   TEXT NOT NULL,
            category TEXT NOT NULL,
            friction TEXT NOT NULL,
            expected TEXT NOT NULL CHECK (expected IN ('pass','expected_fail')),
            note     TEXT
        );
        CREATE TABLE IF NOT EXISTS benchmark_runs (
            id           INTEGER PRIMARY KEY,
            started_at   TEXT NOT NULL,
            finished_at  TEXT,
            heartbeat_at TEXT NOT NULL,
            owner        TEXT NOT NULL CHECK (owner IN ('daemon','cli')),
            paused       INTEGER NOT NULL DEFAULT 0,
            mode         TEXT NOT NULL CHECK (mode IN ('extract','full')),
            state        TEXT NOT NULL CHECK (state IN ('running','done','failed','abandoned')),
            params_json  TEXT NOT NULL,
            summary_json TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_benchmark_runs_state ON benchmark_runs(state, mode);
        CREATE TABLE IF NOT EXISTS benchmark_results (
            run_id        INTEGER NOT NULL REFERENCES benchmark_runs(id) ON DELETE CASCADE,
            target_id     INTEGER NOT NULL REFERENCES benchmark_targets(id),
            extract_ok    INTEGER,
            extract_ms    INTEGER,
            method        TEXT,
            candidates    INTEGER,
            download_ok   INTEGER,
            download_ms   INTEGER,
            transcode_ms  INTEGER,
            verify_ok     INTEGER,
            total_ms      INTEGER,
            error_code    TEXT,
            stream_host   TEXT,
            PRIMARY KEY (run_id, target_id)
        );
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
        for table in ["users", "sessions", "queue", "journalists", "audit_logs", "processed_mail", "mail_checkpoints", "groups", "journalist_groups", "mail_reprocess"] {
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
    fn migration_15_creates_job_evidence() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, None).unwrap();
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('job_evidence')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for c in ["job_id", "path", "bytes", "created_at"] {
            assert!(columns.iter().any(|x| x == c), "job_evidence.{c} missing: {columns:?}");
        }
        assert!(target_version() >= 15);
    }

    #[test]
    fn migration_16_creates_site_stats() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, None).unwrap();
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('site_stats')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for c in [
            "domain", "recent", "consecutive_fail", "last_ok_at", "last_fail_at", "last_code",
            "state", "state_since", "llm_breaker_until", "updated_at",
        ] {
            assert!(columns.iter().any(|x| x == c), "site_stats.{c} missing: {columns:?}");
        }
        assert!(target_version() >= 16);
    }

    #[test]
    fn migration_17_creates_site_canaries() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, None).unwrap();
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('site_canaries')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for c in [
            "id", "domain", "url", "job_id", "added_at", "last_checked_at", "last_ok",
            "last_detail", "fail_count",
        ] {
            assert!(columns.iter().any(|x| x == c), "site_canaries.{c} missing: {columns:?}");
        }
        conn.execute(
            "INSERT INTO site_canaries (domain, url, added_at) VALUES ('a.example', 'https://a.example/1', 'x')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO site_canaries (domain, url, added_at) VALUES ('a.example', 'https://a.example/1', 'x')",
                [],
            )
            .is_err(),
            "url must be unique"
        );
        assert!(target_version() >= 17);
    }

    #[test]
    fn migration_18_creates_held_outputs() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, None).unwrap();
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('held_outputs')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for c in [
            "job_id", "path", "bytes", "sha256", "duration", "created_at", "expires_at",
            "decision", "decided_by", "decided_at", "reason", "delivered_path",
        ] {
            assert!(columns.iter().any(|x| x == c), "held_outputs.{c} missing: {columns:?}");
        }
        assert!(target_version() >= 18);
    }

    fn columns_of(conn: &Connection, table: &str) -> Vec<String> {
        conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// A database at version 18 keeps its rows when 19 is applied, the recipe
    /// tables appear, and deleting a recipe takes its site rows with it.
    #[test]
    fn migration_19_adds_recipe_tables_and_keeps_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 18) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 18);
        conn.execute_batch(
            "INSERT INTO queue (id, url, slug, status) VALUES (5, 'https://a.example/1', '1_ANNA_X', 'PENDING');
             INSERT INTO site_stats (domain, state, state_since, updated_at) VALUES ('a.example', 'healthy', 'x', 'x');
             INSERT INTO held_outputs (job_id, path, bytes, sha256, created_at, expires_at)
                VALUES (5, 'p', 1, 'abc', 'x', 'y');",
        )
        .unwrap();
        for t in ["recipes", "recipe_sites", "recipe_tombstones"] {
            assert!(columns_of(&conn, t).is_empty(), "{t} exists before 19");
        }

        assert_eq!(apply(&mut conn, None).unwrap(), target_version());
        assert!(target_version() >= 19);

        let count = |t: &str| -> i64 { conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0)).unwrap() };
        assert_eq!((count("queue"), count("site_stats"), count("held_outputs")), (1, 1, 1));
        for (t, cols) in [
            (
                "recipes",
                &["id", "hash", "steps_hash", "scope_kind", "scope_key", "family", "recipe_json", "dsl_version",
                  "origin", "state", "created_at", "retired_at", "retired_reason", "revived_at"][..],
            ),
            (
                "recipe_sites",
                &["recipe_id", "domain", "alpha", "beta", "scored_at", "created_at", "approvals", "rejections",
                  "uses", "consecutive_fail", "pass_streak", "state", "suspended_reason", "suspended_at",
                  "last_used_at", "last_ok_at"][..],
            ),
            ("recipe_tombstones", &["hash", "reason", "at", "steps_hash", "scope_kind", "scope_key"][..]),
            ("recipe_bars", &["steps_hash", "domain", "at"][..]),
        ] {
            let have = columns_of(&conn, t);
            for c in cols {
                assert!(have.iter().any(|x| x == c), "{t}.{c} missing: {have:?}");
            }
        }

        conn.execute_batch(
            "INSERT INTO recipes (id, hash, steps_hash, scope_kind, scope_key, recipe_json, dsl_version, origin, state, created_at)
                VALUES (1, 'h1', 's1', 'site', 'a.example', '{}', 1, 'llm', 'candidate', 'x');
             INSERT INTO recipe_sites (recipe_id, domain, scored_at, created_at) VALUES (1, 'a.example', 'x', 'x');",
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO recipes (hash, steps_hash, scope_kind, scope_key, recipe_json, dsl_version, origin, state, created_at)
                 VALUES ('h1', 's1', 'site', 'b.example', '{}', 1, 'llm', 'candidate', 'x')",
                [],
            )
            .is_err(),
            "hash must be unique"
        );
        assert!(
            conn.execute("INSERT INTO recipe_sites (recipe_id, domain, scored_at, created_at) VALUES (99, 'x', 'x', 'x')", [])
                .is_err(),
            "a site row needs its recipe"
        );
        assert_eq!(count("recipe_sites"), 1);
        conn.execute("DELETE FROM recipes WHERE id = 1", []).unwrap();
        assert_eq!(count("recipe_sites"), 0, "ON DELETE CASCADE must remove the site rows");
    }

    /// A database at version 19 keeps its rows when 20 is applied; the attempts
    /// table appears and goes with its job.
    #[test]
    fn migration_20_adds_repair_attempts_and_deletes_with_the_job() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 19) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 19);
        conn.execute_batch("INSERT INTO queue (id, url, slug, status) VALUES (5, 'https://a.example/1', '1_ANNA_X', 'PENDING');")
            .unwrap();
        assert!(columns_of(&conn, "repair_attempts").is_empty(), "the table exists before 20");

        assert_eq!(apply(&mut conn, None).unwrap(), target_version());
        assert!(target_version() >= 20);
        let have = columns_of(&conn, "repair_attempts");
        for c in [
            "id", "job_id", "started_at", "finished_at", "mode", "rung", "outcome", "recipe_hash", "recipe_id", "domain",
            "llm_calls", "tokens_in", "tokens_out", "candidates_json", "next_index", "duration_ms", "held_url", "mcr_decision",
        ] {
            assert!(have.iter().any(|x| x == c), "repair_attempts.{c} missing: {have:?}");
        }
        let count = |t: &str| -> i64 { conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0)).unwrap() };
        assert_eq!(count("queue"), 1, "existing job rows must survive");
        conn.execute("INSERT INTO repair_attempts (job_id, started_at, mode) VALUES (5, 'x', 'hold')", []).unwrap();
        assert!(
            conn.execute("INSERT INTO repair_attempts (job_id, started_at, mode) VALUES (99, 'x', 'hold')", []).is_err(),
            "an attempt needs its job"
        );
        let (llm, next): (i64, i64) = conn
            .query_row("SELECT llm_calls, next_index FROM repair_attempts", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((llm, next), (0, 0));
        conn.execute("DELETE FROM queue WHERE id = 5", []).unwrap();
        assert_eq!(count("repair_attempts"), 0, "the attempts go with their job");
    }

    /// A database at version 20 with an attempt row keeps it when 21 is
    /// applied; the image columns appear empty (no comparison made).
    #[test]
    fn migration_21_adds_the_card_evidence_columns_and_keeps_the_attempt() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 20) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 20);
        conn.execute_batch(
            "INSERT INTO queue (id, url, slug, status) VALUES (5, 'https://a.example/1', '1_ANNA_X', 'PENDING');
             INSERT INTO repair_attempts (job_id, started_at, mode, outcome, held_url) VALUES (5, 'x', 'hold', 'held', 'https://cdn.a.example/v.mp4');",
        )
        .unwrap();
        for c in ["image_distance", "image_frame_secs", "image_host", "expected_secs"] {
            assert!(!columns_of(&conn, "repair_attempts").iter().any(|x| x == c), "{c} must not exist before 21");
        }

        assert_eq!(apply(&mut conn, None).unwrap(), target_version());
        assert!(target_version() >= 21);
        let have = columns_of(&conn, "repair_attempts");
        for c in ["image_distance", "image_frame_secs", "image_host", "expected_secs"] {
            assert!(have.iter().any(|x| x == c), "repair_attempts.{c} missing: {have:?}");
        }
        let (outcome, url, dist, secs, host, exp): (String, String, Option<i64>, Option<f64>, Option<String>, Option<f64>) = conn
            .query_row(
                "SELECT outcome, held_url, image_distance, image_frame_secs, image_host, expected_secs FROM repair_attempts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!((outcome.as_str(), url.as_str()), ("held", "https://cdn.a.example/v.mp4"), "the row survives");
        assert_eq!((dist, secs, host, exp), (None, None, None, None), "NULL means no comparison");
    }

    /// A database at version 21 keeps its rows when 22 is applied; the two
    /// LLM-limit tables appear empty.
    #[test]
    fn migration_22_adds_the_llm_limit_tables_and_keeps_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 21) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 21);
        conn.execute_batch("INSERT INTO queue (id, url, slug, status) VALUES (5, 'https://a.example/1', '1_ANNA_X', 'PENDING');")
            .unwrap();
        assert!(columns_of(&conn, "llm_usage").is_empty(), "the table exists before 22");

        assert_eq!(apply(&mut conn, None).unwrap(), target_version());
        assert!(target_version() >= 22);
        for (table, cols) in [
            ("llm_usage", vec!["day", "calls", "tokens_in", "tokens_out"]),
            ("repair_llm_sent", vec!["hash", "domain", "at"]),
        ] {
            let have = columns_of(&conn, table);
            for c in cols {
                assert!(have.iter().any(|x| x == c), "{table}.{c} missing: {have:?}");
            }
        }
        let jobs: i64 = conn.query_row("SELECT COUNT(*) FROM queue", [], |r| r.get(0)).unwrap();
        assert_eq!(jobs, 1, "existing job rows must survive");
        conn.execute("INSERT INTO llm_usage (day) VALUES ('2026-10-12')", []).unwrap();
        let calls: i64 = conn.query_row("SELECT calls FROM llm_usage", [], |r| r.get(0)).unwrap();
        assert_eq!(calls, 0);
        assert!(conn.execute("INSERT INTO llm_usage (day) VALUES ('2026-10-12')", []).is_err(), "one row per day");
    }

    /// A database at version 22 keeps its rows when 23 is applied; the skip
    /// table appears empty, one row per (domain, pattern).
    #[test]
    fn migration_23_adds_the_repair_skips_table_and_keeps_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 22) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 22);
        conn.execute_batch("INSERT INTO queue (id, url, slug, status) VALUES (5, 'https://a.example/1', '1_ANNA_X', 'PENDING');")
            .unwrap();
        assert!(columns_of(&conn, "repair_skips").is_empty(), "the table exists before 23");

        assert_eq!(apply(&mut conn, None).unwrap(), target_version());
        assert!(target_version() >= 23);
        let have = columns_of(&conn, "repair_skips");
        for c in ["domain", "pattern", "until"] {
            assert!(have.iter().any(|x| x == c), "repair_skips.{c} missing: {have:?}");
        }
        let jobs: i64 = conn.query_row("SELECT COUNT(*) FROM queue", [], |r| r.get(0)).unwrap();
        assert_eq!(jobs, 1, "existing job rows must survive");
        conn.execute("INSERT INTO repair_skips (domain, pattern, until) VALUES ('a.example', 'cdn.a.example/v.mp4', 'x')", [])
            .unwrap();
        assert!(
            conn.execute("INSERT INTO repair_skips (domain, pattern, until) VALUES ('a.example', 'cdn.a.example/v.mp4', 'y')", []).is_err(),
            "one row per domain and pattern"
        );
    }

    /// A database at version 23 keeps its rows when 24 is applied; the self-heal
    /// report table appears empty, with a null-able recipe hash and origin.
    #[test]
    fn migration_24_adds_the_selfheal_reports_table_and_keeps_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 23) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 23);
        conn.execute_batch("INSERT INTO repair_skips (domain, pattern, until) VALUES ('a.example', 'cdn.a.example/v.mp4', 'x');")
            .unwrap();
        assert!(columns_of(&conn, "selfheal_reports").is_empty(), "the table exists before 24");

        assert_eq!(apply(&mut conn, None).unwrap(), target_version());
        assert!(target_version() >= 24);
        let have = columns_of(&conn, "selfheal_reports");
        for c in ["id", "domain", "at", "outcome", "recipe_hash", "origin", "pages", "detail"] {
            assert!(have.iter().any(|x| x == c), "selfheal_reports.{c} missing: {have:?}");
        }
        let skips: i64 = conn.query_row("SELECT COUNT(*) FROM repair_skips", [], |r| r.get(0)).unwrap();
        assert_eq!(skips, 1, "existing rows must survive");
        conn.execute("INSERT INTO selfheal_reports (domain, at, outcome) VALUES ('a.example', 'x', 'nothing')", []).unwrap();
        let pages: i64 = conn.query_row("SELECT pages FROM selfheal_reports", [], |r| r.get(0)).unwrap();
        assert_eq!(pages, 0, "pages defaults to 0");
    }

    #[test]
    fn migration_25_adds_the_mcr_reason_column_and_keeps_the_attempt() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 24) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO queue (id, url, slug, status) VALUES (5, 'https://a.example/x', '1_A_B', 'PENDING');
             INSERT INTO repair_attempts (job_id, started_at, mode, outcome, mcr_decision) VALUES (5, 'x', 'hold', 'held', 'rejected');",
        )
        .unwrap();
        assert!(!columns_of(&conn, "repair_attempts").iter().any(|x| x == "mcr_reason"), "not before 25");

        assert_eq!(apply(&mut conn, None).unwrap(), target_version());
        assert!(target_version() >= 25);
        assert!(columns_of(&conn, "repair_attempts").iter().any(|x| x == "mcr_reason"));
        let (decision, reason): (String, Option<String>) = conn
            .query_row("SELECT mcr_decision, mcr_reason FROM repair_attempts", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((decision.as_str(), reason), ("rejected", None), "an old row keeps its decision and has no reason");
    }

    #[test]
    fn migration_26_creates_cookie_jars_without_touching_old_data() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 25) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        assert!(conn.prepare("SELECT 1 FROM cookie_jars").is_err(), "not before 26");

        assert_eq!(apply(&mut conn, None).unwrap(), target_version());
        assert!(target_version() >= 26);
        let columns = columns_of(&conn, "cookie_jars");
        for c in ["domain", "sealed", "cookie_count", "expires_min", "updated_at", "updated_by", "last_test_at", "last_test_ok", "last_test_detail", "last_sample_url"] {
            assert!(columns.iter().any(|x| x == c), "cookie_jars.{c} missing: {columns:?}");
        }
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

    /// Article siblings and queued offers from before migration 13 learn
    /// their parent from the notes they were written with (plan P7.6).
    #[test]
    fn earlier_article_siblings_get_their_parent_from_their_notes() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 12) {
            conn.execute_batch(sql).unwrap();
            conn.execute("INSERT INTO schema_version (version, applied_at) VALUES (?, 'x')", rusqlite::params![v])
                .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO queue (id, url, slug, status, notes) VALUES
                (7,  'https://portal.gr/a', '1A_PAPADAKI_X', 'COMPLETED', 'Email: ΘΕΜΑΤΑ'),
                (8,  'https://x.com/i/status/2', '1B_PAPADAKI_X', 'PENDING', 'Also in the article of job #7: https://portal.gr/a'),
                (9,  'https://cdn.portal.gr/m.m3u8', '1C_PAPADAKI_X', 'PENDING', 'Offered from the article of job #7: https://portal.gr/a'),
                (10, 'https://youtu.be/q', '2_PAPADAKI_Y', 'PENDING', NULL),
                (11, 'https://youtu.be/r', '3_PAPADAKI_Y', 'PENDING', 'Also in the article of job #: broken');",
        )
        .unwrap();

        apply(&mut conn, None).unwrap();

        let parent = |id: i64| -> Option<i64> {
            conn.query_row("SELECT parent_job_id FROM queue WHERE id = ?", rusqlite::params![id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(parent(8), Some(7));
        assert_eq!(parent(9), Some(7));
        assert_eq!(parent(7), None);
        assert_eq!(parent(10), None);
        assert_eq!(parent(11), None, "a note without a number must not invent parent 0");

        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('processed_mail')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for c in ["received_at", "from_name", "to_addrs", "cc_addrs", "body_text", "attachments_json", "parse_json"] {
            assert!(columns.iter().any(|x| x == c), "processed_mail.{c} missing: {columns:?}");
        }
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
