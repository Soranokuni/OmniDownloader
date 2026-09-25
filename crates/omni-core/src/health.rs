//! System health and status (plan P6.2, defect W-09).
//!
//! `/api/system/status` reported `mail_status: "Active"` and
//! `llm_status: "Ready"` as string literals. They were true when the code was
//! written and never checked again, so the panel showed a healthy mailbox while
//! the mailbox was refusing the password — which is worse than showing nothing,
//! because an operator who trusts it stops looking there.
//!
//! Everything here is *reported*, not asserted. Each subsystem writes its own
//! last-known outcome into a shared [`HealthState`] as it runs, and the status
//! endpoint reads it. That way the panel reflects what actually happened rather
//! than what a synthetic probe at request time would say — a mail poll that
//! succeeded 20 seconds ago is stronger evidence than a fresh connection
//! attempt made because someone opened a web page.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Overall verdict, in the order a monitor cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// Working.
    Ok,
    /// Degraded but still ingesting. The newsroom can work; someone should
    /// look. A missing LLM or a stale mail poll is this.
    Degraded,
    /// Cannot do the job at all. A missing ffmpeg or an unwritable watchfolder
    /// is this: every job will fail at the same step.
    Down,
}

impl Health {
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Degraded => "degraded",
            Health::Down => "down",
        }
    }

    /// The worse of two verdicts. Used to roll checks up.
    pub fn worse(self, other: Health) -> Health {
        self.max(other)
    }
}

/// One named subsystem check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub state: Health,
    /// One line an operator can act on. Never a secret, never a stack trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When this check last produced a result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Check {
    pub fn ok(detail: impl Into<String>) -> Self {
        Self {
            state: Health::Ok,
            detail: Some(detail.into()),
            last_ok: Some(Utc::now()),
            last_error: None,
        }
    }

    pub fn degraded(detail: impl Into<String>) -> Self {
        Self {
            state: Health::Degraded,
            detail: Some(detail.into()),
            last_ok: None,
            last_error: None,
        }
    }

    pub fn down(detail: impl Into<String>) -> Self {
        Self {
            state: Health::Down,
            detail: Some(detail.into()),
            last_ok: None,
            last_error: None,
        }
    }

    /// Not configured, and that is a legitimate state — a station with no
    /// mailbox is running fine, not degraded. Reported as `ok` with a detail
    /// that says so, because "disabled" showing as a warning trains operators
    /// to ignore warnings.
    pub fn disabled(what: &str) -> Self {
        Self {
            state: Health::Ok,
            detail: Some(format!("{what} is not configured")),
            last_ok: None,
            last_error: None,
        }
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.last_error = Some(error.into());
        self
    }
}

/// Shared, mutable health of the running daemon.
///
/// Cheap to clone; every subsystem holds one and writes its own key.
#[derive(Clone)]
pub struct HealthState {
    inner: Arc<Inner>,
}

struct Inner {
    checks: RwLock<BTreeMap<String, Check>>,
    started: Instant,
    started_at: DateTime<Utc>,
}

/// Well-known check names, so the panel and the writers cannot drift apart.
pub mod checks {
    pub const MAIL: &str = "mail";
    pub const LLM: &str = "llm";
    pub const TOOLS: &str = "tools";
    /// The toolchain made a compliant RDD9 file at start-up.
    pub const ENCODER: &str = "encoder";
    pub const WATCHFOLDER: &str = "watchfolder";
    pub const BROWSER: &str = "browser";
    pub const DISK: &str = "disk";
    pub const QUEUE: &str = "queue";
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                checks: RwLock::new(BTreeMap::new()),
                started: Instant::now(),
                started_at: Utc::now(),
            }),
        }
    }

    /// Record (or replace) a check.
    pub fn set(&self, name: &str, check: Check) {
        if let Ok(mut guard) = self.inner.checks.write() {
            guard.insert(name.to_string(), check);
        }
    }

    /// Record a check only if it would change the reported state or detail.
    ///
    /// The mail watcher polls every 20 seconds; rewriting an identical `ok`
    /// each time would churn `last_ok` and make "when did this last work?"
    /// meaningless during an outage — the timestamp would keep advancing while
    /// nothing worked. This keeps the original success time.
    pub fn set_if_changed(&self, name: &str, check: Check) {
        if let Ok(mut guard) = self.inner.checks.write() {
            if let Some(existing) = guard.get(name) {
                if existing.state == check.state && existing.detail == check.detail {
                    return;
                }
            }
            guard.insert(name.to_string(), check);
        }
    }

    pub fn get(&self, name: &str) -> Option<Check> {
        self.inner.checks.read().ok()?.get(name).cloned()
    }

    pub fn all(&self) -> BTreeMap<String, Check> {
        self.inner
            .checks
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// The worst state across every check.
    pub fn overall(&self) -> Health {
        self.inner
            .checks
            .read()
            .map(|g| {
                g.values()
                    .fold(Health::Ok, |acc, c| acc.worse(c.state))
            })
            .unwrap_or(Health::Ok)
    }

    pub fn uptime_secs(&self) -> u64 {
        self.inner.started.elapsed().as_secs()
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.inner.started_at
    }

    /// The compact form for an external monitor: an overall verdict plus each
    /// check's state, and nothing else.
    ///
    /// Deliberately no details, no paths, no versions, no error strings —
    /// `/api/health` is unauthenticated, and "which tool is missing from which
    /// directory" is a fact about the server that an anonymous caller does not
    /// need. A monitor needs to know *that* something is wrong and who to page;
    /// the operator opens the panel to find out what.
    pub fn summary(&self) -> serde_json::Value {
        let states: BTreeMap<String, &'static str> = self
            .all()
            .into_iter()
            .map(|(name, c)| (name, c.state.as_str()))
            .collect();
        serde_json::json!({
            "status": self.overall().as_str(),
            "checks": states,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_state_is_ok_rather_than_unknown() {
        // Before the first poll of anything. Reporting "down" here would page
        // someone every restart.
        assert_eq!(HealthState::new().overall(), Health::Ok);
    }

    #[test]
    fn the_worst_check_decides_the_overall_verdict() {
        let h = HealthState::new();
        h.set(checks::MAIL, Check::ok("polled 3s ago"));
        h.set(checks::LLM, Check::degraded("endpoint unreachable"));
        assert_eq!(h.overall(), Health::Degraded);

        h.set(checks::TOOLS, Check::down("ffmpeg.exe not found"));
        assert_eq!(h.overall(), Health::Down);

        // ...and recovering the worst one recovers the verdict.
        h.set(checks::TOOLS, Check::ok("4 tools present"));
        assert_eq!(h.overall(), Health::Degraded);
    }

    #[test]
    fn a_disabled_subsystem_is_ok_not_degraded() {
        // A station with no mailbox configured is running correctly. Showing
        // that as a warning teaches operators to ignore warnings.
        let h = HealthState::new();
        h.set(checks::MAIL, Check::disabled("Mailbox"));
        assert_eq!(h.overall(), Health::Ok);
        assert!(h.get(checks::MAIL).unwrap().detail.unwrap().contains("not configured"));
    }

    #[test]
    fn repeating_an_identical_success_does_not_move_last_ok() {
        // "Last succeeded at" has to mean what it says. If a 20-second poll
        // rewrote it on every identical result, it would keep advancing during
        // an outage and answer the wrong question.
        let h = HealthState::new();
        h.set_if_changed(checks::MAIL, Check::ok("polled"));
        let first = h.get(checks::MAIL).unwrap().last_ok.unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        h.set_if_changed(checks::MAIL, Check::ok("polled"));
        assert_eq!(h.get(checks::MAIL).unwrap().last_ok.unwrap(), first);

        // A genuine change does replace it.
        h.set_if_changed(checks::MAIL, Check::degraded("authentication failed"));
        assert_eq!(h.get(checks::MAIL).unwrap().state, Health::Degraded);
    }

    #[test]
    fn the_public_summary_leaks_no_detail() {
        // `/api/health` is unauthenticated. A monitor needs a verdict; it does
        // not need to be told which binary is missing from which directory.
        let h = HealthState::new();
        h.set(
            checks::TOOLS,
            Check::down("ffmpeg.exe not found in D:\\OmniDownloader\\bin"),
        );
        h.set(
            checks::MAIL,
            Check::degraded("IMAP authentication failed for ingest@station.gr"),
        );

        let summary = serde_json::to_string(&h.summary()).unwrap();
        assert!(summary.contains("\"status\":\"down\""), "{summary}");
        assert!(summary.contains("\"tools\":\"down\""), "{summary}");
        assert!(!summary.contains("ffmpeg.exe"), "{summary}");
        assert!(!summary.contains("OmniDownloader"), "{summary}");
        assert!(!summary.contains("ingest@station.gr"), "{summary}");
    }

    #[test]
    fn health_ordering_is_ok_then_degraded_then_down() {
        // `worse` relies on the derived Ord, so the variant order is load
        // bearing: reordering the enum would silently invert the rollup.
        assert!(Health::Ok < Health::Degraded);
        assert!(Health::Degraded < Health::Down);
        assert_eq!(Health::Ok.worse(Health::Down), Health::Down);
        assert_eq!(Health::Down.worse(Health::Ok), Health::Down);
    }
}
