use anyhow::Result;
use omni_core::models::JobStatus;
use omni_core::repository::Repository;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_concurrent_queue_leasing_stress() -> Result<()> {
    let temp_db = NamedTempFile::new()?;
    let repo = Repository::new(temp_db.path())?;

    let total_jobs = 40;

    // Insert jobs with ascending priorities
    for i in 1..=total_jobs {
        let priority = if i <= 10 {
            100 // High priority
        } else if i <= 25 {
            50  // Medium priority
        } else {
            0   // Normal priority
        };

        let slug = format!("{}_TEST_SLUG_{}", i, priority);
        repo.add_job(
            &format!("https://example.com/video/{}", i),
            &slug,
            "PAPADAKI",
            "TOPIC",
            &i.to_string(),
            priority,
            JobStatus::Pending,
            None,
            None,
            None,
        )?;
    }

    let leased_count = Arc::new(AtomicUsize::new(0));
    let leased_jobs = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    // Spawn 8 concurrent worker tasks competing for jobs
    let mut handles = Vec::new();
    for worker_id in 0..8 {
        let repo_clone = repo.clone();
        let counter = leased_count.clone();
        let jobs_collector = leased_jobs.clone();

        handles.push(tokio::spawn(async move {
            let mut local_leased = 0;
            loop {
                match repo_clone.lease_next_pending_job() {
                    Ok(Some(job)) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        local_leased += 1;
                        let mut lock = jobs_collector.lock().await;
                        lock.push((job.id, job.priority, job.slug));
                    }
                    Ok(None) => {
                        // Queue empty
                        break;
                    }
                    Err(e) => {
                        panic!("Worker #{} error leasing: {:?}", worker_id, e);
                    }
                }
                tokio::task::yield_now().await;
            }
            local_leased
        }));
    }

    let mut total_processed_by_workers = 0;
    for h in handles {
        total_processed_by_workers += h.await?;
    }

    assert_eq!(total_processed_by_workers, total_jobs);
    assert_eq!(leased_count.load(Ordering::SeqCst), total_jobs);

    let jobs = leased_jobs.lock().await;
    assert_eq!(jobs.len(), total_jobs);

    // Verify uniqueness: no job was leased twice
    let mut ids: Vec<i64> = jobs.iter().map(|(id, _, _)| *id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total_jobs, "Every job must be leased exactly once");

    // Verify priority scheduling: high priority (100) jobs should appear before low priority (0)
    let first_10_priorities: Vec<i32> = jobs.iter().take(10).map(|(_, p, _)| *p).collect();
    for p in first_10_priorities {
        assert_eq!(p, 100, "Top 10 leased jobs must be priority 100");
    }

    Ok(())
}
