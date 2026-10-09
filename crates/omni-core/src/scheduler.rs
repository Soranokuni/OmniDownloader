//! Maintenance scheduling (plan P6.6).
//!
//! The previous scheduler was `if now.format("%H") == "03"` inside an hourly
//! tick. It could not express three things that matter on a newsroom machine:
//!
//! * **A time that is not on the hour.** Everything wanting 03:00 meant the
//!   yt-dlp download and the adblock download raced each other on the same
//!   link.
//! * **A missed run.** An MCR workstation that was switched off overnight came
//!   back with no update and no record that one had been skipped; the next
//!   attempt was the following night.
//! * **An outcome.** There was no way to answer "did last night's update
//!   work?" other than grepping a log that did not exist (W-11).
//!
//! Persisting `next_run` per task answers all three: a task whose time has
//! passed runs at the next tick, whenever the daemon happens to start, and the
//! row records what happened.
//!
//! Scheduling here is deliberately simple — daily at a wall-clock time, with
//! optional jitter. Cron expressions would be more general and would need a
//! parser, a dependency, and an operator who can read them.

use std::time::Duration;

use chrono::{DateTime, Local, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

/// How often a task repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Every day at a local wall-clock time.
    ///
    /// Local, not UTC, because the point of 03:00 is that it is the quiet hour
    /// in the newsroom — and a station on summer time expects it to stay the
    /// quiet hour when the clocks change.
    DailyAt { hour: u32, minute: u32 },
    /// Every `n` days at a local wall-clock time. Used for the monthly VACUUM.
    EveryNDaysAt { days: i64, hour: u32, minute: u32 },
}

impl Cadence {
    /// The next occurrence strictly after `after`.
    pub fn next_after(&self, after: DateTime<Utc>) -> DateTime<Utc> {
        let local = after.with_timezone(&Local);
        match *self {
            Cadence::DailyAt { hour, minute } => next_daily(local, hour, minute, 1),
            Cadence::EveryNDaysAt { days, hour, minute } => {
                next_daily(local, hour, minute, days.max(1))
            }
        }
    }
}

fn next_daily(
    after: DateTime<Local>,
    hour: u32,
    minute: u32,
    step_days: i64,
) -> DateTime<Utc> {
    let time = NaiveTime::from_hms_opt(hour.min(23), minute.min(59), 0)
        .unwrap_or_else(|| NaiveTime::from_hms_opt(3, 0, 0).unwrap());

    let mut day = after.date_naive();
    loop {
        // `and_local_timezone` is ambiguous across a DST transition (the hour
        // repeats) and impossible inside the skipped hour. Taking the earliest
        // valid mapping, and stepping to the next day when there is none, is
        // the behaviour that keeps a nightly task nightly instead of silently
        // skipping the day the clocks move.
        if let Some(candidate) = day.and_time(time).and_local_timezone(Local).earliest() {
            let candidate_utc = candidate.with_timezone(&Utc);
            if candidate_utc > after.with_timezone(&Utc) {
                return candidate_utc;
            }
        }
        day = day.succ_opt().unwrap_or(day);
        if step_days > 1 {
            // For an N-day cadence, jump the whole interval once past today.
            day = day
                .checked_add_signed(chrono::Duration::days(step_days - 1))
                .unwrap_or(day);
        }
    }
}

/// A scheduled maintenance task.
#[derive(Debug, Clone)]
pub struct TaskSpec {
    /// Stable key; it is the primary key of `scheduled_tasks` and appears in
    /// the admin panel, so renaming one loses its history.
    pub name: &'static str,
    /// Shown to operators.
    pub description: &'static str,
    pub cadence: Cadence,
    /// Random delay added to each run, up to this much.
    ///
    /// Only meaningful for tasks that talk to a shared external service: every
    /// install firing at exactly 03:00 is a self-inflicted thundering herd on
    /// the GitHub release endpoint, and a rate-limited failure then looks like
    /// a network fault.
    pub jitter: Duration,
}

/// Outcome of one run, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskOutcome {
    Ok,
    /// Ran and declined to do anything — the yt-dlp update finding the pool
    /// busy, for instance. Not a failure, and it must not read as one.
    Skipped,
    Failed,
}

impl TaskOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskOutcome::Ok => "ok",
            TaskOutcome::Skipped => "skipped",
            TaskOutcome::Failed => "failed",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "ok" => TaskOutcome::Ok,
            "skipped" => TaskOutcome::Skipped,
            _ => TaskOutcome::Failed,
        }
    }
}

/// A task row as the admin panel sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatus {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_outcome: Option<String>,
    pub last_error: Option<String>,
    pub last_ms: Option<i64>,
}

/// The maintenance tasks this build knows about.
///
/// Times are spread rather than stacked: the two that download run twenty
/// minutes apart so they do not compete for the same link, and the two that
/// touch the database run after both, so a purge never races a swap.
pub fn default_tasks() -> Vec<TaskSpec> {
    vec![
        TaskSpec {
            name: "ytdl_update",
            description: "Check for a new yt-dlp, verify its checksum and stage it",
            cadence: Cadence::DailyAt { hour: 3, minute: 0 },
            jitter: Duration::from_secs(20 * 60),
        },
        TaskSpec {
            name: "adblock_update",
            description: "Refresh the ad and tracker blocklists",
            cadence: Cadence::DailyAt { hour: 3, minute: 30 },
            jitter: Duration::from_secs(10 * 60),
        },
        TaskSpec {
            name: "retention",
            description: "Delete expired archive copies, orphaned temp files, old login records and the text of old mail",
            cadence: Cadence::DailyAt { hour: 4, minute: 0 },
            jitter: Duration::ZERO,
        },
        TaskSpec {
            name: "selfcheck",
            description: "Check that the browser works and that a video can still be found at each self-check link",
            // After the yt-dlp update (03:00-03:20) and the blocklists, so it
            // checks what the newsroom will use that day.
            cadence: Cadence::DailyAt { hour: 5, minute: 15 },
            jitter: Duration::ZERO,
        },
        TaskSpec {
            name: "vacuum",
            description: "Compact the database",
            cadence: Cadence::EveryNDaysAt {
                days: 30,
                hour: 4,
                minute: 30,
            },
            jitter: Duration::ZERO,
        },
    ]
}

/// A random delay up to `spread`.
pub fn jitter(spread: Duration) -> Duration {
    use rand::Rng;
    let max = spread.as_secs();
    if max == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs(rand::thread_rng().gen_range(0..=max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Timelike};

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn a_daily_task_lands_on_the_configured_time_today_or_tomorrow() {
        let cadence = Cadence::DailyAt {
            hour: 3,
            minute: 30,
        };

        // Before today's slot: today.
        let next = cadence.next_after(local(2026, 6, 10, 1, 0));
        assert_eq!(next.with_timezone(&Local).hour(), 3);
        assert_eq!(next.with_timezone(&Local).minute(), 30);
        assert_eq!(next.with_timezone(&Local).day(), 10);

        // After it: tomorrow, same time.
        let next = cadence.next_after(local(2026, 6, 10, 9, 0));
        assert_eq!(next.with_timezone(&Local).day(), 11);
        assert_eq!(next.with_timezone(&Local).hour(), 3);
    }

    #[test]
    fn the_next_run_is_strictly_after_now_so_a_task_cannot_loop_on_itself() {
        // Called immediately after a run completes at exactly the scheduled
        // time. `>=` here would schedule the same instant again and the task
        // would run in a tight loop for a whole minute.
        let cadence = Cadence::DailyAt { hour: 3, minute: 0 };
        let at_the_slot = local(2026, 6, 10, 3, 0);
        let next = cadence.next_after(at_the_slot);
        assert!(next > at_the_slot);
        assert_eq!(next.with_timezone(&Local).day(), 11);
    }

    #[test]
    fn an_n_day_cadence_skips_ahead_rather_than_running_daily() {
        let cadence = Cadence::EveryNDaysAt {
            days: 30,
            hour: 4,
            minute: 30,
        };
        let from = local(2026, 6, 10, 9, 0);
        let next = cadence.next_after(from);
        let gap = (next - from).num_days();
        assert!(gap >= 29 && gap <= 31, "gap was {gap} days");
    }

    #[test]
    fn the_times_are_spread_so_two_downloads_do_not_race() {
        // yt-dlp and the blocklists both fetch from the internet. Stacking them
        // on the same minute was how the old `hour == "03"` check behaved.
        let tasks = default_tasks();
        let mut slots: Vec<(u32, u32)> = tasks
            .iter()
            .map(|t| match t.cadence {
                Cadence::DailyAt { hour, minute } => (hour, minute),
                Cadence::EveryNDaysAt { hour, minute, .. } => (hour, minute),
            })
            .collect();
        slots.sort();
        slots.dedup();
        assert_eq!(slots.len(), tasks.len(), "two tasks share a slot");
    }

    #[test]
    fn only_the_tasks_that_hit_a_shared_service_are_jittered() {
        // Jitter on a purely local task buys nothing and makes "when did it
        // run?" harder to answer.
        for task in default_tasks() {
            let expects_jitter = matches!(task.name, "ytdl_update" | "adblock_update");
            assert_eq!(
                !task.jitter.is_zero(),
                expects_jitter,
                "{} jitter setting is wrong",
                task.name
            );
        }
    }

    #[test]
    fn jitter_stays_within_its_window_and_zero_means_zero() {
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
        for _ in 0..200 {
            assert!(jitter(Duration::from_secs(600)).as_secs() <= 600);
        }
    }

    #[test]
    fn task_names_are_unique_because_they_are_a_primary_key() {
        let tasks = default_tasks();
        let mut names: Vec<&str> = tasks.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), tasks.len());
    }
}
