//! Alerts beyond the open tab (plan P11.14): the Windows Event Log, where an
//! IT administrator or a monitoring agent looks when nobody has the panel open.
//!
//! Three conditions, each written once when it starts and once when it ends,
//! never on every tick:
//! - a health line turning red (Error; back to normal: Information);
//! - jobs waiting for a person (review, manual download, approval) longer than
//!   `alerts.waiting_for_person_mins` (Warning, one entry for the jobs that
//!   crossed the limit since the last tick);
//! - the mailbox not answering for 10 minutes (Error; answering again:
//!   Information). The `mail` line's own red is reported by this rule, not
//!   twice.
//!
//! [`Alerter`] is pure: it is given what the daemon knows now and returns the
//! entries to write. A restart forgets what was written, so a condition that
//! still holds is written once more after it, never more often.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use omni_core::health::{checks, Check, Health};
use omni_core::repository::WaitingForPerson;
use omni_service::eventlog::EventLevel;

/// How long the mailbox may go unanswered before it is an alert.
pub(crate) const MAIL_SILENT: Duration = Duration::minutes(10);
/// Jobs named in one waiting entry; the rest are counted.
const JOBS_NAMED: usize = 10;

/// One Event Log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alert {
    pub level: EventLevel,
    pub message: String,
}

/// What the daemon knows at one tick.
pub(crate) struct Snapshot<'a> {
    pub now: DateTime<Utc>,
    pub checks: &'a BTreeMap<String, Check>,
    /// The last mail poll that succeeded, if any since start-up.
    pub last_mail_poll: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    /// `None` when they could not be read: what was written stands.
    pub waiting: Option<&'a [WaitingForPerson]>,
}

/// What has been written, so each condition is written once per transition.
pub(crate) struct Alerter {
    waiting_limit: Duration,
    red: BTreeSet<String>,
    mail_silent: bool,
    overdue: BTreeSet<i64>,
}

fn status_words(status: &str) -> &'static str {
    match status {
        "REQUIRES_REVIEW" => "in review",
        "MANUAL_DOWNLOAD" => "manual download",
        "AWAITING_APPROVAL" => "awaiting approval",
        _ => "waiting",
    }
}

impl Alerter {
    /// Alerts for jobs waiting longer than `waiting_mins` (read as 1..=1440).
    pub(crate) fn new(waiting_mins: i64) -> Self {
        Self {
            waiting_limit: Duration::minutes(waiting_mins.clamp(1, 1440)),
            red: BTreeSet::new(),
            mail_silent: false,
            overdue: BTreeSet::new(),
        }
    }

    /// The entries to write for `now`.
    pub(crate) fn tick(&mut self, s: &Snapshot) -> Vec<Alert> {
        let mut out = Vec::new();
        self.health_lines(s, &mut out);
        self.mailbox(s, &mut out);
        self.waiting(s, &mut out);
        out
    }

    fn health_lines(&mut self, s: &Snapshot, out: &mut Vec<Alert>) {
        let red_now: BTreeSet<String> =
            s.checks.iter().filter(|(name, c)| name.as_str() != checks::MAIL && c.state == Health::Down).map(|(n, _)| n.clone()).collect();
        for name in red_now.difference(&self.red) {
            let detail = s.checks.get(name).and_then(|c| c.detail.clone()).unwrap_or_default();
            out.push(Alert {
                level: EventLevel::Error,
                message: format!("Health line «{name}» is red: {detail}. See the admin panel, Overview."),
            });
        }
        for name in self.red.difference(&red_now) {
            out.push(Alert { level: EventLevel::Info, message: format!("Health line «{name}» is no longer red.") });
        }
        self.red = red_now;
    }

    fn mailbox(&mut self, s: &Snapshot, out: &mut Vec<Alert>) {
        let mail = s.checks.get(checks::MAIL);
        let failing = mail.is_some_and(|c| c.state != Health::Ok);
        let answered = s.last_mail_poll.unwrap_or(s.started_at);
        let silent = failing && s.now - answered >= MAIL_SILENT;
        if silent && !self.mail_silent {
            let minutes = (s.now - answered).num_minutes();
            let detail = mail.and_then(|c| c.detail.clone()).unwrap_or_default();
            out.push(Alert {
                level: EventLevel::Error,
                message: format!("The mailbox has not answered for {minutes} minutes; new mail is not being ingested: {detail}"),
            });
            self.mail_silent = true;
        } else if self.mail_silent && !failing {
            out.push(Alert { level: EventLevel::Info, message: "The mailbox answers again.".into() });
            self.mail_silent = false;
        }
    }

    fn waiting(&mut self, s: &Snapshot, out: &mut Vec<Alert>) {
        let Some(waiting) = s.waiting else {
            return;
        };
        let overdue: Vec<&WaitingForPerson> = waiting.iter().filter(|w| s.now - w.since >= self.waiting_limit).collect();
        let new: Vec<&&WaitingForPerson> = overdue.iter().filter(|w| !self.overdue.contains(&w.id)).collect();
        if !new.is_empty() {
            let named: Vec<String> = new
                .iter()
                .take(JOBS_NAMED)
                .map(|w| format!("#{} {} ({}, {} min)", w.id, w.slug, status_words(&w.status), (s.now - w.since).num_minutes()))
                .collect();
            let more = new.len().saturating_sub(JOBS_NAMED);
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            out.push(Alert {
                level: EventLevel::Warning,
                message: format!(
                    "{} video(s) have waited for a person for more than {} minutes: {}{more}. See the MCR desk.",
                    new.len(),
                    self.waiting_limit.num_minutes(),
                    named.join(", ")
                ),
            });
        }
        // A job that stopped waiting is forgotten: if it comes back, that is a new wait.
        self.overdue = overdue.iter().map(|w| w.id).collect();
    }
}

/// Write `alert` to the file log, and to the Event Log when running as the service.
pub(crate) fn write(alert: &Alert, event_log: bool) {
    match alert.level {
        EventLevel::Error => tracing::error!(target: "alerts", "{}", alert.message),
        EventLevel::Warning => tracing::warn!(target: "alerts", "{}", alert.message),
        EventLevel::Info => tracing::info!(target: "alerts", "{}", alert.message),
    }
    if event_log {
        omni_service::eventlog::report(alert.level, &alert.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-19T10:00:00Z").unwrap().with_timezone(&Utc)
    }

    struct World {
        checks: BTreeMap<String, Check>,
        last_mail_poll: Option<DateTime<Utc>>,
        waiting: Vec<WaitingForPerson>,
        readable: bool,
    }

    impl World {
        fn new() -> Self {
            Self { checks: BTreeMap::new(), last_mail_poll: Some(t0()), waiting: Vec::new(), readable: true }
        }
        fn tick(&self, a: &mut Alerter, mins: i64) -> Vec<Alert> {
            a.tick(&Snapshot {
                now: t0() + Duration::minutes(mins),
                checks: &self.checks,
                last_mail_poll: self.last_mail_poll,
                started_at: t0(),
                waiting: self.readable.then_some(self.waiting.as_slice()),
            })
        }
        fn job(&mut self, id: i64, status: &str, since_min: i64) {
            self.waiting.push(WaitingForPerson { id, slug: format!("{id}_ANNA_TEST"), status: status.into(), since: t0() + Duration::minutes(since_min) });
        }
    }

    #[test]
    fn a_red_line_is_written_once_when_it_turns_red_and_once_when_it_recovers() {
        let mut a = Alerter::new(15);
        let mut w = World::new();
        w.checks.insert(checks::DISK.into(), Check::degraded("8 GB free"));
        assert!(w.tick(&mut a, 0).is_empty(), "amber is not an alert");
        w.checks.insert(checks::DISK.into(), Check::down("1 GB free on temp"));
        let first = w.tick(&mut a, 1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].level, EventLevel::Error);
        assert!(first[0].message.contains("«disk» is red: 1 GB free on temp"), "{first:?}");
        for m in 2..30 {
            assert!(w.tick(&mut a, m).is_empty(), "every tick after the first is quiet");
        }
        w.checks.insert(checks::DISK.into(), Check::ok("40 GB free"));
        let back = w.tick(&mut a, 31);
        assert_eq!(back, vec![Alert { level: EventLevel::Info, message: "Health line «disk» is no longer red.".into() }]);
        assert!(w.tick(&mut a, 32).is_empty());
        // Red again: a new transition, written again.
        w.checks.insert(checks::DISK.into(), Check::down("1 GB free on temp"));
        assert_eq!(w.tick(&mut a, 33).len(), 1);
    }

    #[test]
    fn the_mailbox_is_an_alert_after_ten_silent_minutes_and_once() {
        let mut a = Alerter::new(15);
        let mut w = World::new();
        w.checks.insert(checks::MAIL.into(), Check::degraded("connection refused"));
        assert!(w.tick(&mut a, 9).is_empty(), "nine minutes is not yet ten");
        let alert = w.tick(&mut a, 10);
        assert_eq!(alert.len(), 1, "{alert:?}");
        assert_eq!(alert[0].level, EventLevel::Error);
        assert!(alert[0].message.contains("not answered for 10 minutes") && alert[0].message.contains("connection refused"));
        // The mail line going red is the same condition: not written twice.
        w.checks.insert(checks::MAIL.into(), Check::down("connection refused"));
        for m in 11..60 {
            assert!(w.tick(&mut a, m).is_empty(), "minute {m}");
        }
        w.checks.insert(checks::MAIL.into(), Check::ok("polled"));
        w.last_mail_poll = Some(t0() + Duration::minutes(60));
        assert_eq!(w.tick(&mut a, 60), vec![Alert { level: EventLevel::Info, message: "The mailbox answers again.".into() }]);
        assert!(w.tick(&mut a, 61).is_empty());
    }

    #[test]
    fn a_mailbox_that_is_not_configured_or_answers_is_never_an_alert() {
        let mut a = Alerter::new(15);
        let mut w = World::new();
        w.checks.insert(checks::MAIL.into(), Check::disabled("Mailbox"));
        w.last_mail_poll = None;
        assert!(w.tick(&mut a, 120).is_empty());
        w.checks.insert(checks::MAIL.into(), Check::ok("polled"));
        assert!(w.tick(&mut a, 240).is_empty(), "a quiet but answering mailbox is fine");
    }

    #[test]
    fn a_mailbox_that_never_answered_since_the_start_counts_from_the_start() {
        let mut a = Alerter::new(15);
        let mut w = World::new();
        w.last_mail_poll = None;
        w.checks.insert(checks::MAIL.into(), Check::down("authentication failed"));
        assert!(w.tick(&mut a, 5).is_empty());
        assert_eq!(w.tick(&mut a, 10).len(), 1);
    }

    #[test]
    fn jobs_waiting_for_a_person_past_the_limit_are_written_once_each() {
        let mut a = Alerter::new(15);
        let mut w = World::new();
        w.job(12, "REQUIRES_REVIEW", 0);
        w.job(13, "AWAITING_APPROVAL", 5);
        assert!(w.tick(&mut a, 14).is_empty(), "under fifteen minutes");
        let first = w.tick(&mut a, 15);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].level, EventLevel::Warning);
        assert_eq!(first[0].message, "1 video(s) have waited for a person for more than 15 minutes: #12 12_ANNA_TEST (in review, 15 min). See the MCR desk.");
        let second = w.tick(&mut a, 20);
        assert!(second.len() == 1 && second[0].message.contains("#13 13_ANNA_TEST (awaiting approval, 15 min)") && !second[0].message.contains("#12"), "{second:?}");
        for m in 21..200 {
            assert!(w.tick(&mut a, m).is_empty(), "minute {m}");
        }
        // A tick whose list could not be read changes nothing: no repeat afterwards.
        w.readable = false;
        assert!(w.tick(&mut a, 200).is_empty());
        w.readable = true;
        assert!(w.tick(&mut a, 200).is_empty(), "nothing is written again after an unreadable tick");
        // Job 12 is handled; if it comes back to review later, that is a new wait.
        w.waiting.retain(|j| j.id != 12);
        assert!(w.tick(&mut a, 200).is_empty());
        w.job(12, "MANUAL_DOWNLOAD", 200);
        assert!(w.tick(&mut a, 214).is_empty());
        assert_eq!(w.tick(&mut a, 215).len(), 1);
    }

    #[test]
    fn many_waiting_jobs_are_one_entry_naming_ten() {
        let mut a = Alerter::new(15);
        let mut w = World::new();
        for id in 1..=14 {
            w.job(id, "REQUIRES_REVIEW", 0);
        }
        let all = w.tick(&mut a, 30);
        assert_eq!(all.len(), 1);
        assert!(all[0].message.starts_with("14 video(s)") && all[0].message.contains("and 4 more"), "{all:?}");
        assert!(!all[0].message.contains("http"), "no addresses");
    }

    #[test]
    fn the_limit_is_read_within_bounds() {
        assert_eq!(Alerter::new(0).waiting_limit, Duration::minutes(1));
        assert_eq!(Alerter::new(100_000).waiting_limit, Duration::minutes(1440));
        assert_eq!(Alerter::new(15).waiting_limit, Duration::minutes(15));
    }
}
