//! The overnight self-heal's reports (plan P10.14): kept 30 days, at most 500 rows,
//! newest first, hosts and a recipe hash only. Invented sites.

use anyhow::Result;
use chrono::{Duration, Utc};
use tempfile::TempDir;

use omni_core::repository::{Repository, SELFHEAL_MAX_ROWS};

fn repo(dir: &TempDir) -> Repository {
    Repository::new(dir.path().join("omni.db")).unwrap()
}

#[test]
fn reports_come_back_newest_first_within_the_window() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let now = Utc::now();
    r.record_selfheal("a.example", "nothing", None, None, 0, Some("2 page(s) looked at"), now - Duration::hours(30))?;
    r.record_selfheal("b.example", "found", Some("abcdef0123456789"), Some("llm"), 2, None, now - Duration::hours(2))?;
    r.record_selfheal("c.example", "outage", None, None, 0, None, now - Duration::hours(1))?;

    let day = r.selfheal_since(now - Duration::hours(24))?;
    let names: Vec<&str> = day.iter().map(|x| x.domain.as_str()).collect();
    assert_eq!(names, ["c.example", "b.example"], "the 30-hour-old run is outside 24 h");
    let found = &day[1];
    assert_eq!((found.outcome.as_str(), found.origin.as_deref(), found.pages), ("found", Some("llm"), 2));
    assert_eq!(found.recipe_hash.as_deref(), Some("abcdef0123456789"));
    assert!(found.at.is_some());
    assert_eq!(r.selfheal_since(now - Duration::days(30))?.len(), 3);
    Ok(())
}

#[test]
fn reports_older_than_thirty_days_are_purged_and_the_table_never_holds_more_than_the_cap() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let now = Utc::now();
    r.record_selfheal("old.example", "nothing", None, None, 0, None, now - Duration::days(31))?;
    r.record_selfheal("new.example", "nothing", None, None, 0, None, now - Duration::days(29))?;
    assert_eq!(r.purge_selfheal(now)?, 1, "only the 31-day-old run goes");
    let left = r.selfheal_since(now - Duration::days(60))?;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].domain, "new.example");

    for i in 0..(SELFHEAL_MAX_ROWS + 20) {
        r.record_selfheal(&format!("s{i}.example"), "nothing", None, None, 0, None, now - Duration::minutes(1))?;
    }
    let all = r.selfheal_since(now - Duration::days(60))?;
    assert_eq!(all.len() as i64, SELFHEAL_MAX_ROWS, "at most 500 rows");
    assert!(all.iter().any(|x| x.domain == format!("s{}.example", SELFHEAL_MAX_ROWS + 19)), "the newest stay");
    assert!(all.iter().all(|x| x.domain != "new.example"), "the oldest went first");
    Ok(())
}
