//! Login rate limiting (plan P2.2).
//!
//! Two independent counters, because they stop different attacks:
//!
//! * **per IP** — one host grinding through a password list. Cheap to trip, and
//!   the honest operator who mistypes twice never notices it.
//! * **per account** — the same password tried against one account from many
//!   hosts. Only this one sees a distributed guess, and it has to be keyed on
//!   the account rather than the source.
//!
//! In-memory on purpose. A restart clears the counters, which is the right
//! trade for a single-node daemon: persisting them would let an attacker lock a
//! newsroom account out across restarts, and the audit trail of every attempt
//! lives in `login_attempts` regardless.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failures older than this are forgotten for the per-IP counter.
pub const IP_WINDOW: Duration = Duration::from_secs(5 * 60);
/// ...and for the per-account counter.
pub const ACCOUNT_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Entries are dropped once they are empty; this bounds the map against an
/// attacker cycling source addresses faster than the sweep.
const MAX_TRACKED_KEYS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Locked out; `retry_after` is what goes in the `Retry-After` header.
    Deny { retry_after: Duration },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

#[derive(Default)]
struct Buckets {
    by_ip: HashMap<IpAddr, Vec<Instant>>,
    by_account: HashMap<String, Vec<Instant>>,
}

pub struct LoginRateLimiter {
    buckets: Mutex<Buckets>,
    ip_limit: u32,
    account_limit: u32,
    ip_window: Duration,
    account_window: Duration,
}

impl LoginRateLimiter {
    pub fn new(ip_limit: u32, account_limit: u32) -> Self {
        Self {
            buckets: Mutex::new(Buckets::default()),
            ip_limit,
            account_limit,
            ip_window: IP_WINDOW,
            account_window: ACCOUNT_WINDOW,
        }
    }

    /// Same, with explicit windows. Tests use it to avoid sleeping for minutes.
    pub fn with_windows(
        ip_limit: u32,
        account_limit: u32,
        ip_window: Duration,
        account_window: Duration,
    ) -> Self {
        Self {
            buckets: Mutex::new(Buckets::default()),
            ip_limit,
            account_limit,
            ip_window,
            account_window,
        }
    }

    /// May this login attempt proceed? Read-only — it records nothing.
    pub fn check(&self, ip: Option<IpAddr>, email: &str) -> Decision {
        self.check_at(ip, email, Instant::now())
    }

    fn check_at(&self, ip: Option<IpAddr>, email: &str, now: Instant) -> Decision {
        let key = account_key(email);
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(ip) = ip {
            if let Some(hits) = buckets.by_ip.get(&ip) {
                if let Some(retry) = over_limit(hits, self.ip_limit, self.ip_window, now) {
                    return Decision::Deny { retry_after: retry };
                }
            }
        }
        if let Some(hits) = buckets.by_account.get(&key) {
            if let Some(retry) = over_limit(hits, self.account_limit, self.account_window, now) {
                return Decision::Deny { retry_after: retry };
            }
        }
        let _ = &mut buckets;
        Decision::Allow
    }

    /// Record a failed attempt against both counters.
    pub fn record_failure(&self, ip: Option<IpAddr>, email: &str) {
        self.record_failure_at(ip, email, Instant::now());
    }

    fn record_failure_at(&self, ip: Option<IpAddr>, email: &str, now: Instant) {
        let key = account_key(email);
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(ip) = ip {
            let hits = buckets.by_ip.entry(ip).or_default();
            hits.retain(|t| now.duration_since(*t) < self.ip_window);
            hits.push(now);
        }
        let hits = buckets.by_account.entry(key).or_default();
        hits.retain(|t| now.duration_since(*t) < self.account_window);
        hits.push(now);

        if buckets.by_ip.len() + buckets.by_account.len() > MAX_TRACKED_KEYS {
            sweep(&mut buckets, self.ip_window, self.account_window, now);
        }
    }

    /// Clear both counters for an account after a successful login, so an
    /// operator who fumbled their password four times is not then locked out
    /// by the fifth attempt of the day an hour later.
    pub fn record_success(&self, ip: Option<IpAddr>, email: &str) {
        let key = account_key(email);
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        buckets.by_account.remove(&key);
        if let Some(ip) = ip {
            buckets.by_ip.remove(&ip);
        }
    }
}

/// Accounts are matched case-insensitively, exactly as the login lookup does.
/// Keying on the raw input would let `Admin@x` and `admin@x` each get their own
/// budget.
fn account_key(email: &str) -> String {
    email.trim().to_lowercase()
}

/// `Some(retry_after)` when `hits` within `window` reach `limit`.
fn over_limit(hits: &[Instant], limit: u32, window: Duration, now: Instant) -> Option<Duration> {
    if limit == 0 {
        return None; // 0 means "no limit configured", not "block everything".
    }
    let live: Vec<Instant> = hits
        .iter()
        .copied()
        .filter(|t| now.duration_since(*t) < window)
        .collect();
    if (live.len() as u32) < limit {
        return None;
    }
    // Unlocked when the oldest live failure ages out of the window.
    let oldest = live.iter().min().copied()?;
    Some(window.saturating_sub(now.duration_since(oldest)))
}

fn sweep(buckets: &mut Buckets, ip_window: Duration, account_window: Duration, now: Instant) {
    buckets.by_ip.retain(|_, hits| {
        hits.retain(|t| now.duration_since(*t) < ip_window);
        !hits.is_empty()
    });
    buckets.by_account.retain(|_, hits| {
        hits.retain(|t| now.duration_since(*t) < account_window);
        !hits.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn the_nth_failure_locks_the_ip_out() {
        let rl = LoginRateLimiter::new(5, 100);
        for _ in 0..4 {
            assert!(rl.check(ip("10.0.0.1"), "a@b.gr").is_allowed());
            rl.record_failure(ip("10.0.0.1"), "a@b.gr");
        }
        assert!(rl.check(ip("10.0.0.1"), "a@b.gr").is_allowed());
        rl.record_failure(ip("10.0.0.1"), "a@b.gr");
        assert!(!rl.check(ip("10.0.0.1"), "a@b.gr").is_allowed());

        // A different host is unaffected -- the per-IP counter must not turn
        // one attacker into a station-wide denial of service.
        assert!(rl.check(ip("10.0.0.2"), "a@b.gr").is_allowed());
    }

    #[test]
    fn the_account_counter_catches_a_distributed_guess() {
        // Ten different source addresses, one failure each: every per-IP
        // counter stays at 1, so only the account counter can see this.
        let rl = LoginRateLimiter::new(5, 10);
        for i in 0..10 {
            let addr = format!("10.0.{}.{}", i, i + 1);
            let addr = ip(&addr);
            assert!(rl.check(addr, "target@station.gr").is_allowed());
            rl.record_failure(addr, "target@station.gr");
        }
        assert!(!rl.check(ip("10.9.9.9"), "target@station.gr").is_allowed());
        // Another account from the same fresh IP is still fine.
        assert!(rl.check(ip("10.9.9.9"), "someone@station.gr").is_allowed());
    }

    #[test]
    fn the_account_counter_is_case_insensitive() {
        let rl = LoginRateLimiter::new(100, 3);
        for _ in 0..3 {
            rl.record_failure(ip("10.0.0.1"), "Admin@Station.GR");
        }
        assert!(!rl.check(ip("10.0.0.2"), "admin@station.gr").is_allowed());
    }

    #[test]
    fn failures_age_out_of_the_window() {
        let rl = LoginRateLimiter::with_windows(
            3,
            100,
            Duration::from_secs(300),
            Duration::from_secs(3600),
        );
        let t0 = Instant::now();
        for _ in 0..3 {
            rl.record_failure_at(ip("10.0.0.1"), "a@b.gr", t0);
        }
        assert!(!rl.check_at(ip("10.0.0.1"), "a@b.gr", t0).is_allowed());
        // Still locked just before the window closes...
        let almost = t0 + Duration::from_secs(299);
        assert!(!rl.check_at(ip("10.0.0.1"), "a@b.gr", almost).is_allowed());
        // ...and free again after it.
        let after = t0 + Duration::from_secs(301);
        assert!(rl.check_at(ip("10.0.0.1"), "a@b.gr", after).is_allowed());
    }

    #[test]
    fn retry_after_counts_down_to_when_the_oldest_failure_expires() {
        let rl = LoginRateLimiter::with_windows(
            2,
            100,
            Duration::from_secs(300),
            Duration::from_secs(3600),
        );
        let t0 = Instant::now();
        rl.record_failure_at(ip("10.0.0.1"), "a@b.gr", t0);
        rl.record_failure_at(ip("10.0.0.1"), "a@b.gr", t0 + Duration::from_secs(10));

        match rl.check_at(ip("10.0.0.1"), "a@b.gr", t0 + Duration::from_secs(60)) {
            Decision::Deny { retry_after } => {
                assert_eq!(retry_after.as_secs(), 240);
            }
            Decision::Allow => panic!("should be locked out"),
        }
    }

    #[test]
    fn a_successful_login_clears_the_operators_earlier_fumbles() {
        let rl = LoginRateLimiter::new(5, 10);
        for _ in 0..4 {
            rl.record_failure(ip("10.0.0.1"), "a@b.gr");
        }
        rl.record_success(ip("10.0.0.1"), "a@b.gr");
        for _ in 0..4 {
            assert!(rl.check(ip("10.0.0.1"), "a@b.gr").is_allowed());
            rl.record_failure(ip("10.0.0.1"), "a@b.gr");
        }
        assert!(rl.check(ip("10.0.0.1"), "a@b.gr").is_allowed());
    }

    #[test]
    fn an_unknown_client_address_still_counts_against_the_account() {
        // No ConnectInfo (a proxy we do not trust, an odd transport): the
        // per-IP counter has nothing to key on, so the account counter is the
        // only defence and must still work.
        let rl = LoginRateLimiter::new(5, 3);
        for _ in 0..3 {
            rl.record_failure(None, "a@b.gr");
        }
        assert!(!rl.check(None, "a@b.gr").is_allowed());
    }

    #[test]
    fn a_zero_limit_disables_that_counter_rather_than_blocking_everything() {
        let rl = LoginRateLimiter::new(0, 0);
        for _ in 0..50 {
            rl.record_failure(ip("10.0.0.1"), "a@b.gr");
        }
        assert!(rl.check(ip("10.0.0.1"), "a@b.gr").is_allowed());
    }
}
