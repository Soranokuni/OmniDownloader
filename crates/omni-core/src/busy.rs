//! Jobs a worker is still busy with, in this process (plans P7.20, P1.15).
//!
//! MCR can cancel a running job, which takes its lease away at once. The
//! worker holding it is told through the entry's token and stops its tool
//! within about a second; until it has returned, the job must not be queued
//! again, or two workers would share one workspace.
//!
//! Entries are per `(job, owner)`: if the reaper requeues a stalled job and a
//! second worker leases it while the first is still unwinding, each has its
//! own entry and token, and one finishing does not free the other.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

type Entries = HashMap<(i64, String), CancellationToken>;

/// Shared handle. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct BusyJobs {
    entries: Arc<Mutex<Entries>>,
}

impl BusyJobs {
    pub fn new() -> Self {
        Self::default()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether any worker is still busy with `id`.
    pub fn contains(&self, id: i64) -> bool {
        self.map().keys().any(|(job, _)| *job == id)
    }

    /// Cancel the token of every worker busy with `id`; how many there were.
    pub fn cancel(&self, id: i64) -> usize {
        let map = self.map();
        let mut n = 0;
        for ((job, _), token) in map.iter() {
            if *job == id {
                token.cancel();
                n += 1;
            }
        }
        n
    }

    /// Mark `id` busy for `owner` until the guard is dropped, also on panic or
    /// early return.
    pub fn guard(&self, id: i64, owner: &str) -> BusyGuard {
        let token = CancellationToken::new();
        self.map().insert((id, owner.to_string()), token.clone());
        BusyGuard { busy: self.clone(), id, owner: owner.to_string(), token }
    }
}

/// Removes its own entry from [`BusyJobs`] when dropped.
#[derive(Debug)]
pub struct BusyGuard {
    busy: BusyJobs,
    id: i64,
    owner: String,
    token: CancellationToken,
}

impl BusyGuard {
    /// The job run's token: cancelled by MCR's cancel or by a lost lease.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.busy.map().remove(&(self.id, self.owner.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_the_guard_frees_the_job_even_on_panic() {
        let busy = BusyJobs::new();
        {
            let _g = busy.guard(7, "w1");
            assert!(busy.contains(7));
            assert!(!busy.contains(8));
        }
        assert!(!busy.contains(7));

        let b2 = busy.clone();
        let _ = std::thread::spawn(move || {
            let _g = b2.guard(9, "w1");
            panic!("worker died");
        })
        .join();
        assert!(!busy.contains(9), "a panic left the id stuck");
    }

    #[test]
    fn two_owners_of_one_job_do_not_free_each_other() {
        let busy = BusyJobs::new();
        let a = busy.guard(5, "A");
        let b = busy.guard(5, "B");
        drop(a);
        assert!(busy.contains(5), "A finishing freed the job while B works");
        drop(b);
        assert!(!busy.contains(5));
    }

    #[test]
    fn cancel_fires_every_owners_token_of_that_job_only() {
        let busy = BusyJobs::new();
        let a = busy.guard(5, "A");
        let b = busy.guard(5, "B");
        let other = busy.guard(6, "A");
        assert_eq!(busy.cancel(5), 2);
        assert!(a.token().is_cancelled());
        assert!(b.token().is_cancelled());
        assert!(!other.token().is_cancelled());
        assert_eq!(busy.cancel(99), 0);
    }
}
