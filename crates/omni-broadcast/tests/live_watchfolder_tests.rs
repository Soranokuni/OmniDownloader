//! P13.2: the watchfolder changes between jobs, never under one. The change
//! waits on the update gate for running jobs; with none running it switches,
//! and every finished video still waiting for the old folder goes to review.

use std::path::PathBuf;
use std::time::Duration;

use omni_broadcast::pipeline::{switch_watchfolder, DeliveryPaths, WatchfolderSwitch};
use omni_core::models::{JobStage, JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use omni_core::update_gate::UpdateGate;
use tempfile::TempDir;

const OWNER: &str = "newsroom1:100:1";
const SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

struct Fixture {
    dir: TempDir,
    repo: Repository,
    gate: UpdateGate,
    paths: DeliveryPaths,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let paths = DeliveryPaths::new(dir.path().join("old-watch"), None);
        Self { dir, repo, gate: UpdateGate::new(), paths }
    }

    fn queue(&self, n: u32) -> i64 {
        self.repo
            .enqueue(&NewJob::new(&format!("https://example.invalid/v{n}"), &format!("{n}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id()
    }

    /// Job `n`, finished, its rename to the old folder marked, waiting for it in the queue.
    fn parked_pending(&self, n: u32) -> i64 {
        let id = self.queue(n);
        assert_eq!(self.repo.lease_job(OWNER, 3600).unwrap().expect("leased").id, id);
        assert!(self.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
        let marker = self.dir.path().join("old-watch").join(format!("{n}_ANNA_TEST.mxf"));
        assert!(self.repo.mark_delivering(id, OWNER, &marker.to_string_lossy(), 4096).unwrap());
        let ready = self.dir.path().join("temp").join("jobs").join(id.to_string()).join("out.mxf");
        assert!(self.repo.park_for_delivery(id, OWNER, &ready.to_string_lossy(), 4096, SHA, 12.5).unwrap());
        assert!(self.repo.requeue_waiting_delivery(id, OWNER, chrono::Duration::zero(), "waits").unwrap());
        id
    }

    fn new_folder(&self) -> PathBuf {
        self.dir.path().join("new-watch")
    }

    fn delivering_to(&self, id: i64) -> Option<String> {
        self.repo.delivery_marker(id).unwrap().map(|(path, _, _)| path)
    }
}

/// A change nobody replaced.
fn wanted() -> bool {
    true
}

/// P13.2b: a change replaced by a newer save while it drained is never
/// applied: the folder stays, no waiting video goes to review, the gate opens.
#[tokio::test]
async fn a_change_replaced_while_it_drained_touches_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let f = Fixture::new();
    let parked = f.parked_pending(1);
    let running = f.gate.try_enter().expect("a job holds a pass");
    let replaced = Arc::new(AtomicBool::new(false));
    let (gate, paths, repo, new, r) = (f.gate.clone(), f.paths.clone(), f.repo.clone(), f.new_folder(), replaced.clone());
    let switching = tokio::spawn(async move {
        switch_watchfolder(&gate, &paths, &repo, new, Duration::from_secs(30), move || !r.load(Ordering::SeqCst)).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.gate.is_paused(), "it is draining");
    replaced.store(true, Ordering::SeqCst); // a newer save
    drop(running);

    let out = switching.await.unwrap().unwrap();
    assert_eq!(out, WatchfolderSwitch::Superseded);
    assert_eq!(f.paths.watchfolder(), f.dir.path().join("old-watch"), "the replaced folder is never used");
    assert_eq!(f.repo.get_job(parked).unwrap().unwrap().status, JobStatus::Pending, "nothing goes to review for it");
    assert!(!f.gate.is_paused(), "the queue runs again");
}

/// P13.2b: a zero drain is a passive check: with a job running it answers
/// Busy at once and leaves the gate open; with none it switches.
#[tokio::test]
async fn a_zero_drain_checks_without_holding_jobs_back() {
    let f = Fixture::new();
    let running = f.gate.try_enter().expect("a job holds a pass");
    let out = tokio::time::timeout(Duration::from_secs(1), switch_watchfolder(&f.gate, &f.paths, &f.repo, f.new_folder(), Duration::ZERO, wanted))
        .await
        .expect("answers at once")
        .unwrap();
    assert_eq!(out, WatchfolderSwitch::Busy);
    assert!(!f.gate.is_paused());
    drop(running);
    let out = switch_watchfolder(&f.gate, &f.paths, &f.repo, f.new_folder(), Duration::ZERO, wanted).await.unwrap();
    assert_eq!(out, WatchfolderSwitch::Switched { reviewed: vec![] });
    assert_eq!(f.paths.watchfolder(), f.new_folder());
}

#[tokio::test]
async fn while_a_job_runs_the_change_waits_and_changes_nothing() {
    let f = Fixture::new();
    let parked = f.parked_pending(1);
    let running = f.gate.try_enter().expect("a job holds a pass");

    let out = switch_watchfolder(&f.gate, &f.paths, &f.repo, f.new_folder(), Duration::from_millis(100), wanted).await.unwrap();
    assert_eq!(out, WatchfolderSwitch::Busy);
    assert_eq!(f.paths.watchfolder(), f.dir.path().join("old-watch"), "the folder of the running job is kept");
    assert_eq!(f.repo.get_job(parked).unwrap().unwrap().status, JobStatus::Pending);
    assert!(!f.gate.is_paused(), "the queue runs again after a drain that timed out");
    drop(running);
}

#[tokio::test]
async fn new_jobs_wait_while_it_drains_and_it_switches_once_the_last_job_ends() {
    let f = Fixture::new();
    let running = f.gate.try_enter().expect("a job holds a pass");
    let (gate, paths, repo, new) = (f.gate.clone(), f.paths.clone(), f.repo.clone(), f.new_folder());
    let switching = tokio::spawn(async move { switch_watchfolder(&gate, &paths, &repo, new, Duration::from_secs(30), wanted).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.gate.try_enter().is_none(), "no new job is leased while the change waits");
    assert_eq!(f.paths.watchfolder(), f.dir.path().join("old-watch"), "not under a running job");
    drop(running);
    let out = switching.await.unwrap().unwrap();
    assert_eq!(out, WatchfolderSwitch::Switched { reviewed: vec![] });
    assert_eq!(f.paths.watchfolder(), f.new_folder());
    assert!(f.gate.try_enter().is_some(), "the queue runs again");
}

#[tokio::test]
async fn with_nothing_running_it_switches_and_waiting_videos_go_to_review() {
    let f = Fixture::new();
    let parked = f.parked_pending(1);
    let plain = f.queue(2);
    let marker_before = f.delivering_to(parked);

    let out = switch_watchfolder(&f.gate, &f.paths, &f.repo, f.new_folder(), Duration::from_secs(5), wanted).await.unwrap();
    assert_eq!(out, WatchfolderSwitch::Switched { reviewed: vec![parked] });
    assert_eq!(f.paths.watchfolder(), f.new_folder());

    let job = f.repo.get_job(parked).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::RequiresReview, "never delivered to the new folder by itself");
    assert_eq!(job.error_code.as_deref(), Some("DELIVERY_UNCONFIRMED"));
    assert!(job.error_message.as_deref().unwrap_or("").contains("old-watch"), "{:?}", job.error_message);
    assert_eq!(f.delivering_to(parked), marker_before, "the marker is kept for the person who checks");
    assert_eq!(f.repo.get_job(plain).unwrap().unwrap().status, JobStatus::Pending, "a job not yet made is untouched");

    // The same folder again: nothing to do, nothing reviewed.
    let again = switch_watchfolder(&f.gate, &f.paths, &f.repo, f.new_folder(), Duration::from_secs(5), wanted).await.unwrap();
    assert_eq!(again, WatchfolderSwitch::Switched { reviewed: vec![] });
}
