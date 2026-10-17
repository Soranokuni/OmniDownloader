//! The LLM rung's limits (plan P10.12): the daily call cap, the 24 h bundle
//! memory, the per-site breaker and what clears it.

use anyhow::Result;
use chrono::{Duration, Utc};
use tempfile::TempDir;

use omni_core::models::NewJob;
use omni_core::repository::{AttemptEnd, Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const DAY: &str = "2026-10-12";

fn repo(dir: &TempDir) -> Repository {
    Repository::new(dir.path().join("omni.db")).unwrap()
}

/// A finished attempt on `domain` that called the model `calls` times and ended `outcome`.
fn attempt(repo: &Repository, n: u32, domain: &str, calls: i64, outcome: &str) {
    let job = repo
        .enqueue(&NewJob::new(&format!("https://{domain}/story-{n}"), &format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    let id = repo.start_attempt(job, "hold", Utc::now()).unwrap();
    let end = AttemptEnd {
        domain: Some(domain.into()),
        outcome: Some(outcome.into()),
        llm_calls: calls,
        tokens_in: 100,
        tokens_out: 20,
        ..Default::default()
    };
    repo.finish_attempt(id, &end, Utc::now()).unwrap();
}

#[test]
fn the_daily_cap_holds_across_a_reopen_and_the_next_call_is_refused() -> Result<()> {
    let dir = TempDir::new()?;
    {
        let repo = repo(&dir);
        for i in 0..9 {
            assert!(repo.reserve_llm_call(DAY, 15)?, "call {i} is under the cap");
        }
    }
    // A restart in the middle of the day does not reset the count.
    let repo = repo(&dir);
    for i in 9..15 {
        assert!(repo.reserve_llm_call(DAY, 15)?, "call {i} is under the cap");
    }
    assert!(!repo.reserve_llm_call(DAY, 15)?, "the 16th call of the day must be refused");
    assert_eq!(repo.llm_usage_for(DAY)?.0, 15, "a refused call is not counted");
    assert!(repo.reserve_llm_call("2026-10-13", 15)?, "the next day starts again");
    assert!(!repo.reserve_llm_call("2026-10-14", 0)?, "a cap of 0 allows nothing");
    Ok(())
}

#[test]
fn concurrent_reservations_never_pass_the_cap() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let granted: usize = (0..8)
        .map(|_| {
            let r = repo.clone();
            std::thread::spawn(move || (0..5).filter(|_| r.reserve_llm_call(DAY, 15).unwrap()).count())
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .sum();
    assert_eq!(granted, 15, "40 tries, a cap of 15");
    Ok(())
}

#[test]
fn tokens_add_up_per_day() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    repo.add_llm_tokens(DAY, 1000, 50)?;
    repo.add_llm_tokens(DAY, 500, 25)?;
    assert_eq!(repo.llm_usage_for(DAY)?, (0, 1500, 75));
    assert_eq!(repo.llm_usage_for("2026-01-01")?, (0, 0, 0));
    Ok(())
}

#[test]
fn a_bundle_is_remembered_for_the_window_and_forgotten_by_retention() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let now = Utc::now();
    assert!(!repo.llm_bundle_sent_since("h1", now - Duration::hours(24))?);
    repo.record_llm_bundle("h1", "news.example", now - Duration::hours(30))?;
    assert!(!repo.llm_bundle_sent_since("h1", now - Duration::hours(24))?, "30 hours ago is outside the 24 h window");
    repo.record_llm_bundle("h1", "news.example", now - Duration::hours(2))?;
    assert!(repo.llm_bundle_sent_since("h1", now - Duration::hours(24))?, "sending again renews the time");
    assert!(!repo.llm_bundle_sent_since("other", now - Duration::hours(24))?);

    repo.add_llm_tokens("2026-01-01", 1, 1)?;
    repo.add_llm_tokens(DAY, 1, 1)?;
    assert_eq!(repo.purge_llm_limits(now - Duration::hours(1), "2026-07-01")?, 2, "the bundle and the old day go");
    assert!(!repo.llm_bundle_sent_since("h1", now - Duration::hours(24))?);
    assert_eq!(repo.llm_usage_for(DAY)?.1, 1, "a recent day stays");
    Ok(())
}

#[test]
fn misses_count_only_model_calls_that_found_nothing() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    attempt(&repo, 1, "news.example", 1, "nothing_verified");
    attempt(&repo, 2, "news.example", 2, "exhausted");
    attempt(&repo, 3, "news.example", 0, "nothing_verified"); // no model call
    attempt(&repo, 4, "news.example", 1, "held"); // found
    attempt(&repo, 5, "news.example", 1, "shadow_found"); // found
    attempt(&repo, 6, "news.example", 1, "lost"); // not the model's miss
    attempt(&repo, 7, "other.example", 1, "nothing_verified"); // another site
    let since = Utc::now() - Duration::hours(24);
    assert_eq!(repo.llm_misses_since("news.example", since)?, 2);
    assert_eq!(repo.llm_misses_since("other.example", since)?, 1);
    assert_eq!(repo.llm_misses_since("news.example", Utc::now() + Duration::hours(1))?, 0, "older than the window");
    Ok(())
}

#[test]
fn the_breaker_opens_for_its_time_and_a_canary_pass_or_a_new_build_closes_it() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    let now = Utc::now();
    assert!(!repo.llm_breaker_open("news.example", now)?, "no row, no breaker");

    repo.open_llm_breaker("news.example", now + Duration::hours(24))?;
    assert!(repo.llm_breaker_open("news.example", now)?);
    assert!(repo.llm_breaker_open("NEWS.example", now)?, "the domain is case-insensitive");
    assert!(!repo.llm_breaker_open("news.example", now + Duration::hours(25))?, "it ends by itself");
    assert!(!repo.llm_breaker_open("other.example", now)?);

    // A canary pass clears it with the failure run.
    repo.reset_site_failures("news.example")?;
    assert!(!repo.llm_breaker_open("news.example", now)?, "a canary pass closes the breaker");

    repo.open_llm_breaker("news.example", now + Duration::hours(24))?;
    repo.open_llm_breaker("other.example", now + Duration::hours(24))?;
    repo.close_llm_breaker("other.example")?;
    assert!(repo.llm_breaker_open("news.example", now)? && !repo.llm_breaker_open("other.example", now)?);
    assert_eq!(repo.close_all_llm_breakers()?, 1);
    assert!(!repo.llm_breaker_open("news.example", now)?, "close_all closes every breaker");
    Ok(())
}

#[test]
fn an_open_breaker_survives_the_idle_site_purge() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    repo.open_llm_breaker("news.example", Utc::now() + Duration::hours(24))?;
    repo.purge_idle_sites(0)?;
    assert!(repo.llm_breaker_open("news.example", Utc::now())?, "the idle purge must not drop an open breaker");
    Ok(())
}

#[test]
fn the_attempt_keeps_the_calls_and_tokens() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = repo(&dir);
    attempt(&repo, 1, "news.example", 2, "nothing_verified");
    let job = repo.latest_attempt(1)?.expect("attempt");
    assert_eq!((job.llm_calls, job.tokens_in, job.tokens_out), (2, 100, 20));
    Ok(())
}
