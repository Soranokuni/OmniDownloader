//! Jobs a worker is still busy with, in this process (plan P7.20).
//!
//! MCR can cancel a running job, which takes its lease away at once, but the
//! worker only notices at its next stage boundary and may still be running a
//! tool in `temp/jobs/{id}/`. Until it has returned, the job must not be
//! queued again, or two workers would share one workspace.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Shared handle. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct BusyJobs {
    ids: Arc<Mutex<HashSet<i64>>>,
}

impl BusyJobs {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self) -> std::sync::MutexGuard<'_, HashSet<i64>> {
        self.ids.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn insert(&self, id: i64) {
        self.set().insert(id);
    }

    pub fn remove(&self, id: i64) {
        self.set().remove(&id);
    }

    pub fn contains(&self, id: i64) -> bool {
        self.set().contains(&id)
    }

    /// Mark `id` busy until the guard is dropped, also on panic or early return.
    pub fn guard(&self, id: i64) -> BusyGuard {
        self.insert(id);
        BusyGuard { busy: self.clone(), id }
    }
}

/// Removes its job from [`BusyJobs`] when dropped.
#[derive(Debug)]
pub struct BusyGuard {
    busy: BusyJobs,
    id: i64,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.busy.remove(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_the_guard_frees_the_job_even_on_panic() {
        let busy = BusyJobs::new();
        {
            let _g = busy.guard(7);
            assert!(busy.contains(7));
            assert!(!busy.contains(8));
        }
        assert!(!busy.contains(7));

        let b2 = busy.clone();
        let _ = std::thread::spawn(move || {
            let _g = b2.guard(9);
            panic!("worker died");
        })
        .join();
        assert!(!busy.contains(9), "a panic left the id stuck");
    }
}
