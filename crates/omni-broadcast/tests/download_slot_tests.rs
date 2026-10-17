//! The download slot is keyed by (job, lease owner): a worker whose lease was
//! reaped can never hand back the slot of the worker that leased the job again.

use std::sync::Arc;
use std::time::Duration;

use omni_broadcast::pipeline::BroadcastEngine;
use omni_core::repository::Repository;
use tokio::sync::Semaphore;

fn engine() -> (tempfile::TempDir, BroadcastEngine) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    let engine = BroadcastEngine::new(
        repo,
        root.join("yt-dlp.exe"),
        root.join("ffmpeg.exe"),
        root.join("ffprobe.exe"),
        root.join("bmxtranswrap.exe"),
        root.join("temp"),
        root.join("watch"),
    );
    (dir, engine)
}

#[tokio::test]
async fn a_stale_owner_cannot_release_the_slot_of_the_worker_that_re_leased_the_job() {
    let (_dir, engine) = engine();
    let downloads = Arc::new(Semaphore::new(2));
    // Worker A holds job 7, stalls, is reaped; worker B leases job 7 again.
    engine.hold_download_slot(7, "worker-a", downloads.clone().acquire_owned().await.unwrap());
    engine.hold_download_slot(7, "worker-b", downloads.clone().acquire_owned().await.unwrap());
    assert_eq!(downloads.available_permits(), 0);

    engine.release_download_slot(7, "worker-a");
    assert!(!engine.holds_download_slot(7, "worker-a"));
    assert!(engine.holds_download_slot(7, "worker-b"), "B lost its slot to A's release");
    assert_eq!(downloads.available_permits(), 1, "exactly B's permit is still held");

    engine.release_download_slot(7, "worker-b");
    assert_eq!(downloads.available_permits(), 2);
}

#[tokio::test]
async fn releasing_twice_is_a_no_op() {
    let (_dir, engine) = engine();
    let downloads = Arc::new(Semaphore::new(2));
    engine.hold_download_slot(7, "worker-a", downloads.clone().acquire_owned().await.unwrap());
    engine.hold_download_slot(8, "worker-a", downloads.clone().acquire_owned().await.unwrap());

    engine.release_download_slot(7, "worker-a");
    engine.release_download_slot(7, "worker-a");
    assert_eq!(downloads.available_permits(), 1, "job 8's permit must survive");
    assert!(engine.holds_download_slot(8, "worker-a"));
}

#[tokio::test]
async fn a_worker_that_lost_its_lease_still_frees_its_own_slot() {
    let (_dir, engine) = engine();
    let downloads = Arc::new(Semaphore::new(2));
    engine.hold_download_slot(7, "worker-a", downloads.clone().acquire_owned().await.unwrap());
    engine.hold_download_slot(7, "worker-b", downloads.clone().acquire_owned().await.unwrap());

    // B finishes first, then A wakes up, finds the lease gone and ends.
    engine.release_download_slot(7, "worker-b");
    assert_eq!(downloads.available_permits(), 1);
    engine.release_download_slot(7, "worker-a");
    assert_eq!(downloads.available_permits(), 2, "A's permit leaked");
}

#[tokio::test]
async fn the_download_cap_is_unchanged_the_next_acquire_waits() {
    let (_dir, engine) = engine();
    let downloads = Arc::new(Semaphore::new(2));
    engine.hold_download_slot(1, "w1", downloads.clone().acquire_owned().await.unwrap());
    engine.hold_download_slot(2, "w2", downloads.clone().acquire_owned().await.unwrap());

    let third = tokio::time::timeout(Duration::from_millis(200), downloads.clone().acquire_owned()).await;
    assert!(third.is_err(), "a third download started past the cap");

    engine.release_download_slot(1, "w1");
    let third = tokio::time::timeout(Duration::from_secs(2), downloads.clone().acquire_owned()).await;
    assert!(third.is_ok(), "a freed slot did not let the waiting download start");
}

#[tokio::test]
async fn a_panicking_worker_still_hands_back_its_download_slot() {
    let (_dir, engine) = engine();
    let downloads = Arc::new(Semaphore::new(2));
    let permit = downloads.clone().acquire_owned().await.unwrap();
    let worker = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine.hold_download_slot(7, "worker-a", permit);
            let _guard = engine.download_slot_guard(7, "worker-a");
            panic!("run_job blew up");
        })
    };
    assert!(worker.await.unwrap_err().is_panic());
    assert_eq!(downloads.available_permits(), 2, "the panicked worker leaked its slot");
    assert!(!engine.holds_download_slot(7, "worker-a"));
}

#[tokio::test]
async fn a_guard_never_drops_another_workers_slot() {
    let (_dir, engine) = engine();
    let downloads = Arc::new(Semaphore::new(2));
    engine.hold_download_slot(7, "worker-b", downloads.clone().acquire_owned().await.unwrap());
    drop(engine.download_slot_guard(7, "worker-a"));
    assert!(engine.holds_download_slot(7, "worker-b"));
    assert_eq!(downloads.available_permits(), 1);
}
