//! Worker-pool contract under load (plan P1.1, defect D-01).
//!
//! The old version of this test leased jobs without any notion of capacity, so
//! it could not have caught the defect it was nominally guarding: the daemon
//! leased a job *before* acquiring a worker permit, and with two workers and
//! twenty pending jobs, eighteen sat in a running state with nobody working on
//! them. The MCR panel showed eighteen phantom downloads while an operator
//! waited for files that were not being produced.
//!
//! This version models the real pool — permit first, then lease — and asserts
//! the invariant that follows: **the number of jobs in RUNNING never exceeds
//! the number of permits.**

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use omni_core::models::{JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use tokio::sync::Semaphore;

const TOTAL_JOBS: usize = 40;
const PERMITS: usize = 2;
const WORKERS: usize = 8;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_jobs_never_exceed_worker_permits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;

    for i in 1..=TOTAL_JOBS {
        let priority = match i {
            1..=10 => 100,
            11..=25 => 50,
            _ => 0,
        };
        repo.enqueue(
            &NewJob {
                priority,
                ..NewJob::new(
                    format!("https://example.gr/video/{i}"),
                    format!("{i}_MCR_ASSET{priority}"),
                    "MCR",
                )
            },
            DEFAULT_DEDUP_WINDOW_HOURS,
        )?;
    }

    let semaphore = Arc::new(Semaphore::new(PERMITS));
    let completed = Arc::new(AtomicUsize::new(0));
    let max_seen_running = Arc::new(AtomicUsize::new(0));

    // An independent observer samples the queue while the workers run, so the
    // invariant is checked against the *database*, not against the workers'
    // own bookkeeping.
    let observer = {
        let repo = repo.clone();
        let max_seen = max_seen_running.clone();
        let completed = completed.clone();
        tokio::spawn(async move {
            while completed.load(Ordering::SeqCst) < TOTAL_JOBS {
                if let Ok(running) = repo.get_jobs_by_status(JobStatus::Running) {
                    max_seen.fetch_max(running.len(), Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    let mut handles = Vec::new();
    for w in 0..WORKERS {
        let repo = repo.clone();
        let semaphore = semaphore.clone();
        let completed = completed.clone();
        handles.push(tokio::spawn(async move {
            let owner = format!("stress-host:{}:{w}", std::process::id());
            loop {
                if completed.load(Ordering::SeqCst) >= TOTAL_JOBS {
                    break;
                }
                // Capacity FIRST. This ordering is the whole point.
                let permit = semaphore.clone().acquire_owned().await.unwrap();

                match repo.lease_job(&owner, 120) {
                    Ok(Some(job)) => {
                        // Stand in for the pipeline.
                        tokio::time::sleep(Duration::from_millis(15)).await;
                        repo.finish(job.id, &owner, JobStatus::Completed, None, None, None)
                            .unwrap();
                        completed.fetch_add(1, Ordering::SeqCst);
                        drop(permit);
                    }
                    Ok(None) => {
                        drop(permit);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(e) => panic!("lease failed: {e:?}"),
                }
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
    observer.await.unwrap();

    let peak = max_seen_running.load(Ordering::SeqCst);
    assert!(
        peak <= PERMITS,
        "{peak} jobs were RUNNING at once with only {PERMITS} worker permits; \
         the pool is leasing work it has no capacity to start (defect D-01)"
    );

    // Every job ran exactly once and finished.
    let done = repo.get_jobs_by_status(JobStatus::Completed)?;
    assert_eq!(done.len(), TOTAL_JOBS, "not every job completed");
    assert!(
        done.iter().all(|j| j.attempts == 1),
        "a job was leased more than once: {:?}",
        done.iter().filter(|j| j.attempts != 1).collect::<Vec<_>>()
    );
    assert!(
        repo.get_jobs_by_status(JobStatus::Pending)?.is_empty(),
        "jobs were left pending"
    );
    assert!(
        repo.get_jobs_by_status(JobStatus::Running)?.is_empty(),
        "a lease was left dangling after every worker stopped"
    );

    Ok(())
}

/// High-priority work must not sit behind a backlog of routine ingests.
#[tokio::test]
async fn higher_priority_jobs_are_leased_first() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;

    // Twenty routine jobs already queued...
    for i in 1..=20 {
        repo.enqueue(
            &NewJob::new(
                format!("https://example.gr/routine/{i}"),
                format!("{i}_MCR_ROUTINE"),
                "MCR",
            ),
            DEFAULT_DEDUP_WINDOW_HOURS,
        )?;
    }
    // ...then a breaking-news item arrives last.
    let urgent = repo
        .enqueue(
            &NewJob {
                priority: 100,
                ..NewJob::new("https://example.gr/breaking", "99_MCR_EKTAKTO", "MCR")
            },
            DEFAULT_DEDUP_WINDOW_HOURS,
        )?
        .job_id();

    let first = repo.lease_job("host:1:0", 120)?.expect("a job");
    assert_eq!(
        first.id, urgent,
        "breaking news queued behind 20 routine jobs"
    );
    Ok(())
}
