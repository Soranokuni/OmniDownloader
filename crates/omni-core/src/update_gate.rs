//! Coordination between the tool updater and the worker pool (plan P2.9).
//!
//! Swapping `yt-dlp.exe` while a job is downloading can pull the executable out
//! from under a running process. The old updater did exactly that, at 03:00,
//! which is not a quiet hour for a newsroom that runs a morning bulletin.
//!
//! The gate is a two-sided latch:
//!
//! * workers take a **pass** for the duration of a job, and stop taking new
//!   ones while an update is pending;
//! * the updater raises the pending flag, waits for the outstanding passes to
//!   drain, swaps, and lowers it.
//!
//! Deliberately simple — a counter and a notification, not a lock. A lock held
//! across a download would make the updater's wait unbounded and would let a
//! stuck job block the gate forever; here the updater gives up after its own
//! timeout and tries again the following night, which is the right failure:
//! late is fine, mid-download is not.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Inner {
    /// Number of workers currently inside a job.
    active: AtomicUsize,
    /// An update wants to swap the binary; stop starting new work.
    pending: AtomicBool,
    /// Signalled whenever `active` reaches zero.
    drained: Notify,
}

/// Shared handle. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct UpdateGate {
    inner: Arc<Inner>,
}

impl UpdateGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while an update is waiting to swap. Workers check this *before*
    /// leasing, so a job is never leased into a gap.
    pub fn is_paused(&self) -> bool {
        self.inner.pending.load(Ordering::SeqCst)
    }

    /// Take a pass for the duration of a job. `None` when an update is pending.
    pub fn try_enter(&self) -> Option<GatePass> {
        if self.is_paused() {
            return None;
        }
        self.inner.active.fetch_add(1, Ordering::SeqCst);
        // Re-check after incrementing: without this, an update that raised the
        // flag between the check and the increment would see a count it
        // believed was zero and swap under a job that had just started.
        if self.is_paused() {
            self.release();
            return None;
        }
        Some(GatePass {
            inner: self.inner.clone(),
        })
    }

    pub fn active(&self) -> usize {
        self.inner.active.load(Ordering::SeqCst)
    }

    fn release(&self) {
        if self.inner.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.drained.notify_waiters();
        }
    }

    /// Stop new work and wait for in-flight jobs, up to `timeout`.
    ///
    /// Returns `true` when the pool drained. On `false` the caller **must**
    /// still call [`Self::resume`] and must not swap: a partial drain is
    /// exactly the situation the gate exists to avoid.
    pub async fn pause_and_drain(&self, timeout: Duration) -> bool {
        self.inner.pending.store(true, Ordering::SeqCst);

        if self.active() == 0 {
            return true;
        }

        let wait = async {
            loop {
                // Register interest before re-reading the count, so a release
                // between the two cannot be missed.
                let notified = self.inner.drained.notified();
                if self.active() == 0 {
                    return;
                }
                notified.await;
            }
        };

        tokio::time::timeout(timeout, wait).await.is_ok()
    }

    /// Let workers start leasing again.
    pub fn resume(&self) {
        self.inner.pending.store(false, Ordering::SeqCst);
    }
}

/// Held for the duration of a job; releases the pass when dropped.
///
/// Drop rather than an explicit call, so a worker that returns early — an
/// error, a cancelled lease, a panic — cannot leave the gate stuck closed and
/// silently stop every future update.
#[derive(Debug)]
pub struct GatePass {
    inner: Arc<Inner>,
}

impl Drop for GatePass {
    fn drop(&mut self) {
        if self.inner.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.drained.notify_waiters();
        }
    }
}

/// Jitter a nightly job by up to ±`spread`.
///
/// Every install firing at exactly 03:00 is a self-inflicted thundering herd on
/// the GitHub release endpoint, and it makes a rate-limited failure look like a
/// network fault.
pub fn nightly_jitter(spread: Duration) -> Duration {
    use rand::Rng;
    let max = spread.as_secs().max(1);
    Duration::from_secs(rand::thread_rng().gen_range(0..=max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_idle_pool_drains_immediately() {
        let gate = UpdateGate::new();
        assert!(gate.pause_and_drain(Duration::from_secs(1)).await);
        assert!(gate.is_paused());
        gate.resume();
        assert!(!gate.is_paused());
    }

    #[tokio::test]
    async fn a_pending_update_stops_new_work_from_starting() {
        let gate = UpdateGate::new();
        assert!(gate.try_enter().is_some());
        gate.pause_and_drain(Duration::from_millis(1)).await;
        assert!(
            gate.try_enter().is_none(),
            "a worker must not start a job while a swap is pending"
        );
        gate.resume();
        assert!(gate.try_enter().is_some());
    }

    #[tokio::test]
    async fn the_updater_waits_for_an_in_flight_job() {
        let gate = UpdateGate::new();
        let pass = gate.try_enter().expect("gate is open");
        assert_eq!(gate.active(), 1);

        let gate2 = gate.clone();
        let waiter = tokio::spawn(async move { gate2.pause_and_drain(Duration::from_secs(5)).await });

        // Still downloading: the updater must not be finished.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "the updater swapped mid-download");

        drop(pass);
        assert!(waiter.await.unwrap(), "the updater should see the pool drain");
        assert_eq!(gate.active(), 0);
    }

    #[tokio::test]
    async fn a_job_that_outlasts_the_timeout_leaves_the_binary_alone() {
        let gate = UpdateGate::new();
        let _pass = gate.try_enter().unwrap();

        // The contract: `false` means "do not swap". A long download should
        // postpone the update, not race it.
        assert!(!gate.pause_and_drain(Duration::from_millis(100)).await);
        assert_eq!(gate.active(), 1);
        gate.resume();
    }

    #[tokio::test]
    async fn a_pass_dropped_on_an_error_path_still_releases_the_gate() {
        // A worker that returns early must not wedge the gate closed forever;
        // that would silently disable every future update.
        let gate = UpdateGate::new();
        {
            let _pass = gate.try_enter().unwrap();
            // ...worker fails here and returns.
        }
        assert_eq!(gate.active(), 0);
        assert!(gate.pause_and_drain(Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn several_workers_all_have_to_finish() {
        let gate = UpdateGate::new();
        let a = gate.try_enter().unwrap();
        let b = gate.try_enter().unwrap();
        assert_eq!(gate.active(), 2);

        let gate2 = gate.clone();
        let waiter = tokio::spawn(async move { gate2.pause_and_drain(Duration::from_secs(5)).await });

        drop(a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "one job is still running");

        drop(b);
        assert!(waiter.await.unwrap());
    }

    #[test]
    fn jitter_stays_inside_its_window() {
        for _ in 0..200 {
            let d = nightly_jitter(Duration::from_secs(1200));
            assert!(d.as_secs() <= 1200);
        }
    }
}
