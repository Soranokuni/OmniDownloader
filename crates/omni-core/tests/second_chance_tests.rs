//! Second chance (P11.5): a job waiting in review goes back to the queue on its
//! own, once per cause, when the cause is fixed; never a job a person acted on.
//! Real SQLite files; every name, link and address is invented.

use anyhow::Result;
use chrono::{Duration, Utc};
use rusqlite::{params, Connection};
use tempfile::TempDir;

use omni_core::models::{JobStatus, NewJob};
use omni_core::repository::{AttemptEnd, Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use omni_core::second_chance::{self, COOKIE_CODES, SECOND_CHANCE_SITE_CODES as SITE_CODES};
use omni_core::site_health::{SiteOutcome, SiteState};
use omni_core::timestamps;

const OWNER: &str = "host:1:0";
const NOTE: &str = "νέα έκδοση yt-dlp";

struct Rig {
    _dir: TempDir,
    db: std::path::PathBuf,
    repo: Repository,
}

fn rig() -> Rig {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("omni.db");
    let repo = Repository::new(&db).unwrap();
    Rig { _dir: dir, db, repo }
}

fn window() -> Duration {
    second_chance::window(second_chance::DEFAULT_WINDOW_MINS)
}

impl Rig {
    fn queue(&self, url: &str) -> i64 {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.repo
            .enqueue(&NewJob::new(url.to_string(), format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id()
    }

    fn lease(&self, id: i64) {
        self.sql("UPDATE queue SET not_before = NULL WHERE id = ?1", id);
        assert!(self.repo.lease_job_by_id(id, OWNER, 180).unwrap(), "job {id} not leased");
    }

    /// Leased and sent to review with `code` by its worker.
    fn fail(&self, id: i64, code: &str) {
        self.lease(id);
        assert!(self.repo.finish(id, OWNER, JobStatus::RequiresReview, Some(code), Some("failed"), None).unwrap());
    }

    /// A new job on `url` waiting in review with `code`.
    fn review(&self, url: &str, code: &str) -> i64 {
        let id = self.queue(url);
        self.fail(id, code);
        id
    }

    fn row(&self, id: i64) -> omni_core::models::Job {
        self.repo.get_job(id).unwrap().unwrap()
    }

    fn sql(&self, sql: &str, id: i64) {
        Connection::open(&self.db).unwrap().execute(sql, params![id]).unwrap();
    }

    fn set_text(&self, column: &str, value: &str, id: i64) {
        Connection::open(&self.db)
            .unwrap()
            .execute(&format!("UPDATE queue SET {column} = ?1 WHERE id = ?2"), params![value, id])
            .unwrap();
    }

    fn notes(&self, id: i64) -> Vec<(String, String)> {
        self.repo
            .get_job_events(id, 200)
            .unwrap()
            .into_iter()
            .filter(|e| e.message.starts_with("Αυτόματη επανάληψη"))
            .map(|e| (e.level, e.message))
            .collect()
    }

    /// Leased, held for approval with one verified output, as the repair ladder leaves it.
    fn held(&self, url: &str) -> i64 {
        let id = self.queue(url);
        self.lease(id);
        let at = self.repo.start_attempt(id, "hold", Utc::now()).unwrap();
        let list = serde_json::json!([{ "url": "https://cdn.news.example/v/a.mp4", "origin": "recipe:x" }]);
        let end = AttemptEnd {
            rung: Some("recipes".into()),
            outcome: Some("held".into()),
            domain: Some("news.example".into()),
            candidates_json: Some(list.to_string()),
            held_url: Some("https://cdn.news.example/v/a.mp4".into()),
            next_index: 0,
            ..Default::default()
        };
        self.repo.finish_attempt(at, &end, Utc::now()).unwrap();
        assert!(self.repo.hold_output(id, OWNER, "temp/held/x/clip.mxf", 10, &"ab".repeat(32), Some(9.0), Utc::now() + Duration::hours(24)).unwrap());
        assert!(self.repo.finish(id, OWNER, JobStatus::AwaitingApproval, None, None, None).unwrap());
        id
    }

    fn ytdlp(&self, cause: &str) -> Vec<i64> {
        self.repo.second_chance(SITE_CODES, None, window(), cause, NOTE).unwrap()
    }
}

#[test]
fn only_matching_jobs_in_review_inside_the_window_are_moved() -> Result<()> {
    let r = rig();
    let matching = r.review("https://news.example/a", "NO_STREAM_FOUND");
    let wrong_code = r.review("https://news.example/b", "NETWORK");
    let pending = r.queue("https://news.example/c");
    r.set_text("error_code", "NO_STREAM_FOUND", pending);
    let old = r.review("https://news.example/d", "VIDEO_UNRESOLVED");
    r.set_text("created_at", &timestamps::format(Utc::now() - Duration::minutes(150)), old);
    // Started afresh by MCR long ago, created even longer ago: the age counts from `retry_from`.
    let restarted_old = r.review("https://news.example/e", "HTTP_403");
    r.set_text("retry_from", &timestamps::format(Utc::now() - Duration::minutes(130)), restarted_old);
    // Created long ago but restarted 30 min ago: inside the window.
    let restarted_young = r.review("https://news.example/f", "UNSUPPORTED_URL");
    r.set_text("created_at", &timestamps::format(Utc::now() - Duration::hours(5)), restarted_young);
    r.set_text("retry_from", &timestamps::format(Utc::now() - Duration::minutes(30)), restarted_young);

    assert_eq!(r.ytdlp("ytdlp:0123456789abcdef"), vec![matching, restarted_young]);
    for id in [wrong_code, old, restarted_old] {
        assert_eq!(r.row(id).status, JobStatus::RequiresReview, "job {id} moved");
        assert!(r.repo.second_chances(id)?.is_empty());
    }
    assert_eq!(r.row(pending).status, JobStatus::Pending);
    assert!(r.repo.second_chances(pending)?.is_empty(), "a job not in review was written");
    Ok(())
}

#[test]
fn a_moved_job_runs_again_from_scratch_and_keeps_why_it_failed() -> Result<()> {
    let r = rig();
    let id = r.queue("https://news.example/a");
    r.lease(id);
    assert!(r.repo.requeue_retry(id, OWNER, Duration::minutes(5), "NETWORK", "reset", 5)?);
    r.fail(id, "NO_STREAM_FOUND");
    let from = timestamps::format(Utc::now() - Duration::minutes(30));
    r.set_text("retry_from", &from, id);
    let before = r.row(id);
    assert_eq!((before.attempts, before.first_error_code.as_deref()), (2, Some("NETWORK")));

    assert_eq!(r.ytdlp("ytdlp:0123456789abcdef"), vec![id]);
    let job = r.row(id);
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!(job.attempts, 0);
    assert_eq!(job.not_before, None);
    assert_eq!((job.lease_owner, job.lease_expires_at), (None, None));
    assert_eq!(job.error_code.as_deref(), Some("NO_STREAM_FOUND"), "the desk still sees why it failed");
    assert_eq!(job.first_error_code.as_deref(), Some("NETWORK"));
    assert!(!job.human_touched, "the system is not a person");
    assert_eq!(job.touch_reason, None);
    assert_eq!(job.retry_from, before.retry_from, "the 2-hour limit still counts from the old start");
    assert_eq!(job.max_attempts, before.max_attempts);
    assert_eq!(r.repo.second_chances(id)?, vec!["ytdlp:0123456789abcdef".to_string()]);
    assert_eq!(r.notes(id), vec![("INFO".to_string(), format!("Αυτόματη επανάληψη: {NOTE}"))]);
    // The worker can take it at once.
    assert!(r.repo.lease_job_by_id(id, OWNER, 180)?);
    Ok(())
}

/// Every operator action sets `human_touched`; the job is then put back into
/// review with a matching code, so only the "no person acted" rule stops it.
#[test]
fn a_job_a_person_acted_on_is_never_moved() -> Result<()> {
    let r = rig();
    let url = |n: u32| format!("https://www.instagram.com/p/case{n}");

    // «Δοκιμή ξανά», then it failed again.
    let retried = r.review(&url(1), "LOGIN_REQUIRED");
    assert!(r.repo.retry_job(retried, None)?);
    r.fail(retried, "LOGIN_REQUIRED");
    // Cancelled.
    let cancelled = r.queue(&url(2));
    r.repo.cancel_job(cancelled)?;
    // Marked done («Το έβαλα στο Dalet»).
    let done = r.review(&url(3), "LOGIN_REQUIRED");
    assert!(r.repo.mark_completed_manually(done)?);
    // Rejected by MCR.
    let rejected = r.held(&url(4));
    assert!(r.repo.reject_held(rejected, "other", "not this one", "desk")?);
    assert_eq!(r.row(rejected).error_code.as_deref(), Some("REJECTED_BY_MCR"));
    // «Λάθος βίντεο»: the flag as `wrong_video_after_auto_delivery` writes it.
    let wrong = r.review(&url(5), "LOGIN_REQUIRED");
    r.sql("UPDATE queue SET human_touched = 1, touch_reason = COALESCE(touch_reason, 'wrong_video') WHERE id = ?1", wrong);

    let touched = [retried, cancelled, done, rejected, wrong];
    for id in touched {
        assert!(r.row(id).human_touched, "job {id} not marked as touched");
        r.sql("UPDATE queue SET status = 'REQUIRES_REVIEW', error_code = 'LOGIN_REQUIRED' WHERE id = ?1", id);
    }
    let untouched = r.review(&url(6), "LOGIN_REQUIRED");

    let moved = r.repo.second_chance(COOKIE_CODES, Some("instagram.com"), window(), "cookies:instagram.com:20261119", "νέα cookies για instagram.com")?;
    assert_eq!(moved, vec![untouched]);
    for id in touched {
        assert_eq!(r.row(id).status, JobStatus::RequiresReview, "job {id} a person acted on was moved");
        assert!(r.notes(id).is_empty());
    }
    Ok(())
}

/// An operator acts on the job after it was given up on and before the
/// trigger runs: the guarded update leaves it, whatever else matches.
#[test]
fn an_operators_action_wins_over_the_trigger() -> Result<()> {
    let r = rig();
    let id = r.review("https://news.example/a", "NO_STREAM_FOUND");
    r.sql("UPDATE queue SET human_touched = 1, touch_reason = 'rename' WHERE id = ?1", id);
    assert!(r.ytdlp("ytdlp:aaaa").is_empty());
    assert_eq!(r.row(id).status, JobStatus::RequiresReview);
    assert!(r.repo.second_chances(id)?.is_empty());
    Ok(())
}

#[test]
fn one_cause_moves_a_job_once_and_another_cause_may_move_it_again() -> Result<()> {
    let r = rig();
    let id = r.review("https://news.example/a", "NO_STREAM_FOUND");
    assert_eq!(r.ytdlp("ytdlp:aaaa"), vec![id]);
    r.fail(id, "NO_STREAM_FOUND");
    assert!(r.ytdlp("ytdlp:aaaa").is_empty(), "the same cause moved the job twice");
    assert_eq!(r.row(id).status, JobStatus::RequiresReview);
    assert_eq!(r.ytdlp("ytdlp:bbbb"), vec![id]);
    assert_eq!(r.repo.second_chances(id)?, vec!["ytdlp:aaaa".to_string(), "ytdlp:bbbb".to_string()]);
    assert_eq!(r.notes(id).len(), 2);
    Ok(())
}

#[test]
fn a_site_moves_only_its_own_jobs() -> Result<()> {
    let r = rig();
    let on_x = r.review("https://x.com/someone/status/1", "LOGIN_REQUIRED");
    let on_twitter = r.review("https://twitter.com/someone/status/2", "RATE_LIMITED");
    let on_instagram = r.review("https://www.instagram.com/p/abc", "LOGIN_REQUIRED");
    let moved = r.repo.second_chance(COOKIE_CODES, Some("x.com"), window(), "cookies:x.com:20261119", "νέα cookies για x.com")?;
    assert_eq!(moved, vec![on_x, on_twitter]);
    assert_eq!(r.row(on_instagram).status, JobStatus::RequiresReview, "another site's job was moved");
    Ok(())
}

/// A job whose video waits for or may already be in the watchfolder, one with
/// a held output, and one tidied off the review tab are left for a person.
#[test]
fn a_job_that_may_be_on_air_or_was_tidied_away_stays() -> Result<()> {
    let r = rig();
    let waiting = r.review("https://news.example/a", "NO_STREAM_FOUND");
    r.sql("UPDATE queue SET delivery_wait = 1 WHERE id = ?1", waiting);
    let marked = r.review("https://news.example/b", "NO_STREAM_FOUND");
    r.set_text("delivering_to", "W:/watch/b.mxf", marked);
    let held = r.held("https://news.example/c");
    r.sql("UPDATE queue SET status = 'REQUIRES_REVIEW', error_code = 'NO_STREAM_FOUND' WHERE id = ?1", held);
    let hidden = r.review("https://news.example/d", "NO_STREAM_FOUND");
    r.set_text("cleared_at", &timestamps::now_string(), hidden);
    assert!(r.ytdlp("ytdlp:aaaa").is_empty());
    Ok(())
}

#[test]
fn a_cause_is_one_line_and_no_codes_move_nothing() -> Result<()> {
    let r = rig();
    let id = r.review("https://news.example/a", "NO_STREAM_FOUND");
    assert!(r.repo.second_chance(SITE_CODES, None, window(), "a\nb", NOTE).is_err());
    assert!(r.repo.second_chance(SITE_CODES, None, window(), " ", NOTE).is_err());
    assert!(r.repo.second_chance(&[], None, window(), "ytdlp:a", NOTE)?.is_empty());
    assert_eq!(r.row(id).status, JobStatus::RequiresReview);
    Ok(())
}

#[test]
fn a_job_moved_by_a_second_chance_and_delivered_counts_as_untouched() -> Result<()> {
    let r = rig();
    let since = Utc::now() - Duration::minutes(1);
    let id = r.review("https://news.example/a", "NO_STREAM_FOUND");
    assert_eq!(r.ytdlp("ytdlp:aaaa"), vec![id]);
    r.lease(id);
    assert!(r.repo.finish(id, OWNER, JobStatus::Completed, None, None, Some("watch/a.mxf"))?);
    let report = r.repo.autonomy_report(since)?;
    assert_eq!((report.totals.delivered, report.totals.delivered_untouched), (1, 1));
    Ok(())
}

// ---- the same link twice (review round 1, F1) ----

/// The journalist re-sent the mail while job 1 waited in review: job 2 for the
/// same link was delivered. Moving job 1 would deliver the link a second time.
#[test]
fn a_job_whose_twin_was_delivered_is_not_moved() -> Result<()> {
    let r = rig();
    let first = r.review("https://news.example/a", "NO_STREAM_FOUND");
    let twin = r.queue("https://news.example/a");
    assert_ne!(first, twin, "enqueue refused the twin");
    r.lease(twin);
    assert!(r.repo.finish(twin, OWNER, JobStatus::Completed, None, None, Some("watch/a.mxf"))?);
    assert!(r.ytdlp("ytdlp:aaaa").is_empty());
    assert_eq!(r.row(first).status, JobStatus::RequiresReview);
    Ok(())
}

/// MCR settled ANNA's job by queuing the same link under its own name, and
/// that job was delivered: moving ANNA's job would put the video on air twice.
#[test]
fn a_job_whose_twin_was_delivered_for_another_journalist_is_not_moved() -> Result<()> {
    let r = rig();
    let first = r.review("https://news.example/a", "NO_STREAM_FOUND");
    let twin = r
        .repo
        .enqueue(&NewJob::new("https://news.example/a", "1_MCR_TEST", "MCR"), DEFAULT_DEDUP_WINDOW_HOURS)?
        .job_id();
    assert_ne!(first, twin);
    r.lease(twin);
    assert!(r.repo.finish(twin, OWNER, JobStatus::Completed, None, None, Some("watch/a.mxf"))?);
    assert!(r.ytdlp("ytdlp:aaaa").is_empty());
    assert_eq!(r.row(first).status, JobStatus::RequiresReview);
    // `enqueue` is unchanged: ANNA may still queue the link herself.
    assert!(r.repo.enqueue(&NewJob::new("https://news.example/a", "2_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?.is_new());
    Ok(())
}

#[test]
fn a_job_whose_twin_is_queued_is_not_moved() -> Result<()> {
    let r = rig();
    let first = r.review("https://news.example/a", "NO_STREAM_FOUND");
    let twin = r.queue("https://news.example/a");
    assert_ne!(first, twin);
    assert!(r.ytdlp("ytdlp:aaaa").is_empty());
    assert_eq!(r.row(first).status, JobStatus::RequiresReview);
    Ok(())
}

#[test]
fn of_two_twins_in_review_one_batch_moves_exactly_one() -> Result<()> {
    let r = rig();
    let first = r.review("https://news.example/a", "NO_STREAM_FOUND");
    let twin = r.review("https://news.example/a", "NO_STREAM_FOUND");
    assert_ne!(first, twin);
    assert_eq!(r.ytdlp("ytdlp:aaaa"), vec![first]);
    assert_eq!(r.row(twin).status, JobStatus::RequiresReview);
    Ok(())
}

// ---- how often (F4) and what may be on air (F5) ----

#[test]
fn a_job_gets_one_site_chance_and_three_chances_in_all() -> Result<()> {
    let r = rig();
    let site = |cause: &str| {
        r.repo
            .second_chance(SITE_CODES, Some("news.example"), window(), cause, "ο ιστότοπος news.example λειτουργεί ξανά")
            .unwrap()
    };
    let id = r.review("https://news.example/a", "NO_STREAM_FOUND");
    assert_eq!(site("site:news.example:20261119 0800"), vec![id]);
    r.fail(id, "NO_STREAM_FOUND");
    assert!(site("site:news.example:20261119 0900").is_empty(), "a flapping site moved the job again");
    assert_eq!(r.ytdlp("ytdlp:aaaa"), vec![id]);
    r.fail(id, "NO_STREAM_FOUND");
    assert_eq!(r.ytdlp("ytdlp:bbbb"), vec![id]);
    r.fail(id, "NO_STREAM_FOUND");
    assert!(r.ytdlp("ytdlp:cccc").is_empty(), "a fourth second chance");
    assert_eq!(r.repo.second_chances(id)?.len(), 3);
    assert_eq!(r.row(id).status, JobStatus::RequiresReview);
    Ok(())
}

#[test]
fn a_job_with_a_delivered_file_or_time_recorded_is_never_moved() -> Result<()> {
    let r = rig();
    let with_file = r.review("https://news.example/a", "NO_STREAM_FOUND");
    r.set_text("file_path", "W:/watch/a.mxf", with_file);
    let with_time = r.review("https://news.example/b", "NO_STREAM_FOUND");
    r.set_text("delivered_at", &timestamps::now_string(), with_time);
    assert!(r.ytdlp("ytdlp:aaaa").is_empty());
    Ok(())
}

// ---- triggers ----

#[test]
fn the_cookie_trigger_moves_the_sites_login_jobs_once_a_day() -> Result<()> {
    let r = rig();
    let login = r.review("https://x.com/someone/status/1", "LOGIN_REQUIRED");
    let extraction = r.review("https://x.com/someone/status/2", "NO_STREAM_FOUND");
    assert_eq!(second_chance::after_cookies(&r.repo, "x.com"), 1);
    assert_eq!(r.row(login).status, JobStatus::Pending);
    assert_eq!(r.row(extraction).status, JobStatus::RequiresReview, "cookies do not fix an extractor");
    assert_eq!(r.repo.second_chances(login)?, vec![second_chance::cookies_cause("x.com", Utc::now())]);
    assert_eq!(r.notes(login)[0].1, "Αυτόματη επανάληψη: νέα cookies για x.com");
    // The jar is saved again the same day: the job that failed again stays.
    r.fail(login, "LOGIN_REQUIRED");
    assert_eq!(second_chance::after_cookies(&r.repo, "x.com"), 0);
    Ok(())
}

#[test]
fn the_ytdlp_trigger_moves_extraction_failures_on_any_site() -> Result<()> {
    let r = rig();
    let a = r.review("https://news.example/a", "NO_STREAM_FOUND");
    let b = r.review("https://www.youtube.com/watch?v=abc", "UNSUPPORTED_URL");
    let login = r.review("https://www.youtube.com/watch?v=def", "LOGIN_REQUIRED");
    assert_eq!(second_chance::after_ytdlp_update(&r.repo, "0123456789abcdef"), 2);
    assert_eq!(r.repo.second_chances(a)?, vec!["ytdlp:0123456789abcdef".to_string()]);
    assert_eq!(r.row(b).status, JobStatus::Pending);
    assert_eq!(r.row(login).status, JobStatus::RequiresReview);
    assert_eq!(second_chance::after_deno_update(&r.repo, "2.5.0"), 0, "nothing left in review to move");
    Ok(())
}

/// A known-good site that degraded and then gives video again: its jobs in
/// review move, once, through the repository write that stores the change.
#[test]
fn a_site_that_works_again_gives_its_jobs_a_second_chance_on_the_transition_only() -> Result<()> {
    let r = rig();
    for _ in 0..6 {
        r.repo.record_site_outcome("news.example", SiteOutcome::Success)?;
    }
    let mine = r.review("https://news.example/a", "NO_STREAM_FOUND");
    let other = r.review("https://other.example/a", "NO_STREAM_FOUND");
    // Healthy to healthy: no change, nothing moves.
    r.repo.record_site_outcome("news.example", SiteOutcome::Success)?;
    assert_eq!(r.row(mine).status, JobStatus::RequiresReview, "a success on a healthy site moved a job");
    for _ in 0..3 {
        r.repo.record_site_outcome("news.example", SiteOutcome::SiteFailure("NO_STREAM_FOUND".into()))?;
    }
    assert_eq!(r.repo.get_site("news.example")?.unwrap().state, SiteState::Degraded);
    assert_eq!(r.row(mine).status, JobStatus::RequiresReview);

    let (before, after) = r.repo.record_site_outcome("news.example", SiteOutcome::Success)?;
    assert_eq!((before, after), (SiteState::Degraded, SiteState::Healthy));
    assert_eq!(r.row(mine).status, JobStatus::Pending);
    let causes = r.repo.second_chances(mine)?;
    assert!(causes.len() == 1 && causes[0].starts_with("site:news.example:"), "{causes:?}");
    assert_eq!(r.notes(mine)[0].1, "Αυτόματη επανάληψη: ο ιστότοπος news.example λειτουργεί ξανά");
    assert_eq!(r.row(other).status, JobStatus::RequiresReview, "another site's job moved");
    Ok(())
}

/// The canary marks a broken site healthy (`set_site_state`).
#[test]
fn a_broken_site_set_healthy_gives_its_jobs_a_second_chance() -> Result<()> {
    let r = rig();
    r.repo.record_site_outcome("news.example", SiteOutcome::Success)?;
    assert!(r.repo.set_site_state("news.example", SiteState::Broken)?);
    let mine = r.review("https://www.news.example/a", "VIDEO_UNRESOLVED");
    assert!(r.repo.set_site_state("news.example", SiteState::Broken)?);
    assert_eq!(r.row(mine).status, JobStatus::RequiresReview);
    assert!(r.repo.set_site_state("news.example", SiteState::Healthy)?);
    assert_eq!(r.row(mine).status, JobStatus::Pending);
    // Healthy again: no transition, nothing written.
    r.fail(mine, "VIDEO_UNRESOLVED");
    assert!(r.repo.set_site_state("news.example", SiteState::Healthy)?);
    assert_eq!(r.row(mine).status, JobStatus::RequiresReview);
    Ok(())
}

#[test]
fn the_repositorys_window_is_the_clamped_config_value() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    assert_eq!(repo.second_chance_window().num_minutes(), 120);
    let retry = |window: i64, max_age: i64| omni_core::config::RetryConfig {
        second_chance_window_mins: window,
        max_age_mins: max_age,
        ..Default::default()
    };
    assert_eq!(repo.clone().with_second_chance_window(&retry(10, 360)).second_chance_window().num_minutes(), 30);
    assert_eq!(repo.clone().with_second_chance_window(&retry(999, 360)).second_chance_window().num_minutes(), 360);
    assert_eq!(repo.with_second_chance_window(&retry(360, 120)).second_chance_window().num_minutes(), 120);
}
