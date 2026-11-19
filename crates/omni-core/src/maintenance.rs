//! The database looks after itself (plan P11.13).
//!
//! * A nightly online backup of `omni.db` into `data\backups\`, made with
//!   SQLite's backup API while the daemon runs, checked with `quick_check`
//!   before it counts, and rotated: the 7 newest plus one for each of the 4
//!   weeks before them.
//! * A weekly `PRAGMA quick_check` of the live database, with its own health
//!   line (`database`).
//! * One health line (`maintenance`) that turns amber while any scheduled task
//!   failed on its last run, and names it.
//!
//! Only files named exactly `omni-YYYY-MM-DD.db` (and their `.tmp`) in the
//! backups folder are ever written or removed here; the taxonomy backups in
//! `backups\taxonomy\` and the pre-migration copies next to the database are
//! not touched.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};

use crate::health::{checks, Check, HealthState};
use crate::repository::Repository;
use crate::scheduler::TaskStatus;

/// The daily backup task's name (`scheduled_tasks`).
pub const BACKUP_TASK: &str = "db_backup";
/// The weekly integrity check's name.
pub const CHECK_TASK: &str = "db_check";
/// Newest backups kept whatever their day.
pub const DAILY_KEPT: usize = 7;
/// Older weeks that keep their newest backup.
pub const WEEKLY_KEPT: usize = 4;
/// How a failed integrity check's error starts, so the health line can tell a
/// damaged database from a check that could not run.
pub const INTEGRITY_PROBLEM: &str = "The database integrity check found problems";

/// `omni-YYYY-MM-DD.db`, the backup of `day`.
pub fn backup_name(day: NaiveDate) -> String {
    format!("omni-{}.db", day.format("%Y-%m-%d"))
}

/// The day of a backup named by [`backup_name`]; `None` for any other name.
pub fn backup_day(name: &str) -> Option<NaiveDate> {
    let date = name.strip_prefix("omni-")?.strip_suffix(".db")?;
    if date.len() != 10 {
        return None;
    }
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    (backup_name(day) == name).then_some(day)
}

/// Which of the backups of `days` go: all but the [`DAILY_KEPT`] newest and,
/// of the older ones, the newest of each of the [`WEEKLY_KEPT`] most recent
/// ISO weeks.
pub fn to_delete(days: &[NaiveDate]) -> Vec<NaiveDate> {
    let mut sorted: Vec<NaiveDate> = days.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
    sorted.reverse();
    let mut weeks_kept = BTreeSet::new();
    let mut gone = Vec::new();
    for (i, day) in sorted.into_iter().enumerate() {
        if i < DAILY_KEPT {
            continue;
        }
        let week = (day.iso_week().year(), day.iso_week().week());
        if weeks_kept.len() < WEEKLY_KEPT && weeks_kept.insert(week) {
            continue;
        }
        gone.push(day);
    }
    gone
}

/// What a nightly backup did.
#[derive(Debug, Clone)]
pub struct BackupReport {
    pub path: PathBuf,
    pub bytes: u64,
    pub removed: usize,
}

/// Room the backup needs on its volume: the database twice over (the new
/// copy, and the old one it replaces until the rename).
fn room_needed(db_bytes: u64) -> u64 {
    db_bytes.saturating_mul(2).saturating_add(64 * 1024 * 1024)
}

/// Back the live database up into `dir` as `today`'s file and rotate (P11.13).
/// The copy is written as `.tmp`, checked with `quick_check`, and only then
/// renamed into place, so a cut or damaged copy never counts as a backup.
/// Blocking: run it off the async threads.
pub fn nightly_backup(repo: &Repository, db_bytes: u64, dir: &Path, today: NaiveDate) -> Result<BackupReport> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not create the backups folder {dir:?}"))?;
    if let Some((free, _)) = crate::selftest::disk_space(dir) {
        let need = room_needed(db_bytes);
        anyhow::ensure!(
            free >= need,
            "not enough space for the database backup: {} MB free, {} MB needed",
            free / (1024 * 1024),
            need / (1024 * 1024)
        );
    }
    // A copy a crash or a stop cut short, with any journal files it left.
    for (name, path) in entries(dir) {
        let base = name.strip_suffix("-wal").or_else(|| name.strip_suffix("-shm")).or_else(|| name.strip_suffix("-journal")).unwrap_or(&name);
        if base.strip_suffix(".tmp").and_then(backup_day).is_some() {
            let _ = std::fs::remove_file(path);
        }
    }

    let name = backup_name(today);
    let path = dir.join(&name);
    let tmp = dir.join(format!("{name}.tmp"));
    let made = repo.backup_database(&tmp).and_then(|()| {
        let found = Repository::quick_check_file(&tmp)?;
        anyhow::ensure!(found == ["ok"], "the backup copy failed its check: {}", found.join("; "));
        Ok(())
    });
    if let Err(e) = made {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context("the database backup failed"));
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("could not put the backup in place as {path:?}"))?;
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let days: Vec<NaiveDate> = entries(dir).iter().filter_map(|(n, _)| backup_day(n)).collect();
    let mut removed = 0;
    for day in to_delete(&days) {
        if std::fs::remove_file(dir.join(backup_name(day))).is_ok() {
            removed += 1;
        }
    }
    Ok(BackupReport { path, bytes, removed })
}

/// The files directly in `dir`, by name.
fn entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect()
}

/// The weekly integrity check (P11.13): `Ok` when `quick_check` says "ok",
/// else an error starting with [`INTEGRITY_PROBLEM`]. Blocking.
pub fn integrity_check(repo: &Repository) -> Result<()> {
    let found = repo.quick_check().context("the database integrity check could not run")?;
    if found == ["ok"] {
        return Ok(());
    }
    anyhow::bail!("{INTEGRITY_PROBLEM}: {}", found.join("; "))
}

fn local_time(at: chrono::DateTime<chrono::Utc>) -> String {
    at.with_timezone(&Local).format("%d/%m %H:%M").to_string()
}

/// The `maintenance` line: amber while an enabled task failed on its last
/// run, naming every such task; green otherwise.
pub fn maintenance_check(tasks: &[TaskStatus]) -> Check {
    let failed: Vec<&str> = tasks
        .iter()
        .filter(|t| t.enabled && t.last_outcome.as_deref() == Some("failed"))
        .map(|t| t.name.as_str())
        .collect();
    if failed.is_empty() {
        Check::ok("every maintenance task worked on its last run")
    } else {
        Check::degraded(format!("maintenance task failed on its last run: {}; see Administration → Maintenance", failed.join(", ")))
    }
}

/// The `database` line from the integrity check's and the backup's rows;
/// `None` until the integrity check has run once. A damaged database is red,
/// a check that could not run amber.
pub fn database_check(tasks: &[TaskStatus]) -> Option<Check> {
    let check = tasks.iter().find(|t| t.name == CHECK_TASK)?;
    let at = check.last_run?;
    let backup = tasks
        .iter()
        .find(|t| t.name == BACKUP_TASK && t.last_outcome.as_deref() == Some("ok"))
        .and_then(|t| t.last_run)
        .map(|b| format!("; last backup {}", local_time(b)))
        .unwrap_or_default();
    Some(match check.last_outcome.as_deref() {
        Some("failed") => {
            let error = check.last_error.clone().unwrap_or_default();
            if error.starts_with(INTEGRITY_PROBLEM) {
                Check::down(format!("{error} ({}); restore the newest backup from data\\backups", local_time(at)))
            } else {
                Check::degraded(format!("the integrity check could not run on {}: {error}", local_time(at)))
            }
        }
        _ => Check::ok(format!("integrity check passed {}{backup}", local_time(at))),
    })
}

/// Set both lines from the task rows. Bookkeeping: an unreadable schedule
/// leaves the lines as they are.
pub fn refresh_health(repo: &Repository, health: &HealthState) {
    let Ok(tasks) = repo.list_scheduled_tasks(&crate::scheduler::default_tasks()) else {
        return;
    };
    health.set_if_changed(checks::MAINTENANCE, maintenance_check(&tasks));
    match database_check(&tasks) {
        Some(c) => health.set_if_changed(checks::DATABASE, c),
        None => health.remove(checks::DATABASE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::Health;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn only_exact_backup_names_are_ours() {
        assert_eq!(backup_name(d("2026-11-19")), "omni-2026-11-19.db");
        assert_eq!(backup_day("omni-2026-11-19.db"), Some(d("2026-11-19")));
        for other in ["omni.db", "omni-2026-11-19.db.tmp", "omni-2026-1-9.db", "xomni-2026-11-19.db", "omni-2026-11-19.db-wal", "omni-2026-13-01.db", "taxonomy"] {
            assert_eq!(backup_day(other), None, "{other}");
        }
    }

    #[test]
    fn seven_daily_and_four_weekly_are_kept() {
        // Sixty nightly backups, one a day, ending on Thursday 2026-11-19.
        let last = d("2026-11-19");
        let days: Vec<NaiveDate> = (0..60).map(|n| last - chrono::Duration::days(n)).collect();
        let gone: BTreeSet<NaiveDate> = to_delete(&days).into_iter().collect();
        let kept: Vec<NaiveDate> = days.iter().copied().filter(|x| !gone.contains(x)).collect();
        assert_eq!(kept.len(), DAILY_KEPT + WEEKLY_KEPT, "{kept:?}");
        // The seven newest...
        assert!(kept[..7].iter().zip(0..).all(|(k, n)| *k == last - chrono::Duration::days(n)));
        // ...then the newest of each of four earlier weeks, all in different ISO weeks.
        let weeks: BTreeSet<_> = kept[7..].iter().map(|k| k.iso_week()).collect();
        assert_eq!(weeks.len(), 4);
        assert_eq!(kept[7], d("2026-11-12"), "the rest of the week the dailies began in");
        assert_eq!(kept[8], d("2026-11-08"), "a Sunday: the newest of its week");
        assert_eq!(kept[10], d("2026-10-25"));
        // A few backups: nothing goes.
        assert!(to_delete(&days[..5]).is_empty());
        assert!(to_delete(&[]).is_empty());
    }

    #[test]
    fn missing_nights_still_keep_one_per_week() {
        // The machine was off most nights: every backup older than the seven newest is the only one of its week.
        let days: Vec<NaiveDate> = (0..20).map(|n| d("2026-11-19") - chrono::Duration::days(n * 7)).collect();
        let gone = to_delete(&days);
        assert_eq!(gone.len(), 20 - 7 - 4);
        assert!(gone.iter().all(|g| *g < days[10]), "the oldest go: {gone:?}");
    }

    fn task(name: &str, outcome: Option<&str>, error: Option<&str>) -> TaskStatus {
        TaskStatus {
            name: name.into(),
            description: String::new(),
            enabled: true,
            next_run: None,
            last_run: outcome.map(|_| chrono::Utc::now()),
            last_outcome: outcome.map(String::from),
            last_error: error.map(String::from),
            last_ms: None,
        }
    }

    #[test]
    fn a_failed_task_turns_the_maintenance_line_amber_and_names_it() {
        let mut tasks = vec![task("retention", Some("ok"), None), task("vacuum", None, None), task(BACKUP_TASK, Some("skipped"), None)];
        assert_eq!(maintenance_check(&tasks).state, Health::Ok);
        tasks.push(task(BACKUP_TASK, Some("failed"), Some("disk full")));
        tasks.push(task("cookie_test", Some("failed"), Some("x")));
        let c = maintenance_check(&tasks);
        assert_eq!(c.state, Health::Degraded);
        let detail = c.detail.unwrap();
        assert!(detail.contains("db_backup, cookie_test"), "{detail}");
        assert!(!detail.contains("retention"), "{detail}");
        // A switched-off task is not reported.
        let mut off = task("adblock_update", Some("failed"), None);
        off.enabled = false;
        assert_eq!(maintenance_check(&[off]).state, Health::Ok);
    }

    #[test]
    fn the_database_line_is_red_only_for_a_damaged_database() {
        assert!(database_check(&[task(CHECK_TASK, None, None)]).is_none(), "not checked yet: no line");
        let ok = database_check(&[task(CHECK_TASK, Some("ok"), None), task(BACKUP_TASK, Some("ok"), None)]).unwrap();
        assert_eq!(ok.state, Health::Ok);
        assert!(ok.detail.as_deref().unwrap().contains("last backup"), "{ok:?}");
        let damaged = format!("{INTEGRITY_PROBLEM}: row 12 missing from index idx_queue_status");
        let red = database_check(&[task(CHECK_TASK, Some("failed"), Some(&damaged))]).unwrap();
        assert_eq!(red.state, Health::Down);
        assert!(red.detail.unwrap().contains("restore the newest backup"));
        let busy = database_check(&[task(CHECK_TASK, Some("failed"), Some("the database integrity check could not run: database is locked"))]).unwrap();
        assert_eq!(busy.state, Health::Degraded);
    }
}
