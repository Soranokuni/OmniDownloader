//! The delivery of a finished video that waited for the watchfolder (P11.2a),
//! through the real engine: `BroadcastEngine::deliver_ready`. No tool runs on
//! this path (nothing is downloaded, encoded or verified again: the bytes are
//! proven to be the verified ones), so the "video" here is any file.

use std::path::{Path, PathBuf};

use chrono::Duration;
use omni_broadcast::pipeline::{BroadcastEngine, ReadyOutcome};
use omni_core::archive::ArchiveSettings;
use omni_core::models::{Job, JobStage, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const OWNER: &str = "newsroom1:100:1";
const SLUG: &str = "1_ANNA_LIMANI";
const VIDEO: &[u8] = b"A FINISHED, VERIFIED XDCAM HD422 MXF (stand-in bytes)";

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: Repository,
    engine: BroadcastEngine,
    watch: PathBuf,
}

/// `blocked`: the share's parent is a file, so the watchfolder cannot be created.
fn rig(archive: bool, blocked: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    if blocked {
        std::fs::write(root.join("share"), b"not a directory").unwrap();
    }
    let watch = root.join("share").join("watch");
    let engine = BroadcastEngine::new(
        repo.clone(),
        root.join("no-yt-dlp.exe"),
        root.join("no-ffmpeg.exe"),
        root.join("no-ffprobe.exe"),
        root.join("no-bmx.exe"),
        root.join("temp"),
        watch.clone(),
    )
    .with_archive(archive.then(|| ArchiveSettings::new(root.join("archive"), 14)));
    Rig { _dir: dir, root, repo, engine, watch }
}

impl Rig {
    fn unblock(&self) {
        let share = self.root.join("share");
        if share.is_file() {
            std::fs::remove_file(share).unwrap();
        }
    }

    fn workspace(&self, id: i64) -> PathBuf {
        self.root.join("temp").join("jobs").join(id.to_string())
    }

    fn watch_files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.watch)
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    fn events(&self, id: i64) -> Vec<String> {
        self.repo.get_job_events(id, 200).unwrap().into_iter().map(|e| e.message).collect()
    }

    /// A job whose verified video (`VIDEO`) waits in its workspace next to its
    /// downloaded source, parked by an earlier run (with `marker` as the rename
    /// it attempted) and leased again for the delivery after the wait.
    fn waiting_job(&self, marker: Option<(&Path, u64)>) -> Job {
        let id = self
            .repo
            .enqueue(&NewJob::new("https://news.example.invalid/story/1", SLUG, "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();
        assert_eq!(self.repo.lease_job(OWNER, 3600).unwrap().unwrap().id, id);
        assert!(self.repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
        let ws = self.workspace(id);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("source.mp4"), b"the downloaded source").unwrap();
        let ready = ws.join(format!("{id}_output.mxf"));
        std::fs::write(&ready, VIDEO).unwrap();
        if let Some((path, bytes)) = marker {
            assert!(self.repo.mark_delivering(id, OWNER, &path.to_string_lossy(), bytes).unwrap());
        }
        let sha = omni_core::held::sha256_file(&ready).unwrap();
        assert!(self.repo.park_for_delivery(id, OWNER, &ready.to_string_lossy(), VIDEO.len() as u64, &sha, 3.0).unwrap());
        assert!(self.repo.requeue_waiting_delivery(id, OWNER, Duration::zero(), "waits").unwrap());
        let job = self.repo.lease_job_when(OWNER, 3600, true).unwrap().expect("the waiting job");
        assert_eq!((job.id, job.stage), (id, JobStage::Deliver));
        job
    }
}

/// Test 4: the share is back: the video is delivered exactly once, byte for
/// byte, the wait ends in the same write that records the file, the source
/// goes to the archive as on the normal path, and the workspace goes.
#[tokio::test]
async fn the_waiting_video_is_delivered_once_byte_for_byte_and_its_source_archived() {
    let r = rig(true, false);
    let job = r.waiting_job(None);
    let id = job.id;
    r.engine.watchfolder_state().set(false);

    assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Delivered);

    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")], "exactly one copy, no staging file");
    assert_eq!(std::fs::read(r.watch.join(format!("{SLUG}.mxf"))).unwrap(), VIDEO);
    let row = r.repo.get_job(id).unwrap().unwrap();
    assert_eq!(row.file_path.as_deref(), Some(r.watch.join(format!("{SLUG}.mxf")).to_string_lossy().as_ref()));
    assert!(!row.delivery_wait);
    assert_eq!((row.ready_path, row.ready_bytes, row.ready_sha256), (None, None, None));
    assert_eq!(row.status, omni_core::models::JobStatus::Running, "the worker finishes it");
    assert!(!r.workspace(id).exists(), "the workspace is removed");
    let archived: Vec<String> = walk(&r.root.join("archive"));
    assert!(archived.iter().any(|n| n == &format!("{id}_{SLUG}.mp4")), "the source is archived: {archived:?}");
    assert!(r.engine.watchfolder_state().reachable(), "a delivery that landed opens the lease gate at once");
}

#[tokio::test]
async fn with_the_archive_off_the_waiting_video_is_delivered_and_the_workspace_removed() {
    let r = rig(false, false);
    let job = r.waiting_job(None);
    assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Delivered);
    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")]);
    assert!(!r.workspace(job.id).exists());
    assert!(!r.root.join("archive").exists());
}

/// Test 5 / risk (a): the rename of the earlier attempt landed although it
/// looked failed. The watchfolder has the file at the marker with its size:
/// completed with that file, no second copy, no `_2`.
#[tokio::test]
async fn a_rename_that_landed_before_the_wait_is_never_copied_again() {
    let r = rig(true, false);
    std::fs::create_dir_all(&r.watch).unwrap();
    let landed = r.watch.join(format!("{SLUG}.mxf"));
    std::fs::write(&landed, VIDEO).unwrap();
    let job = r.waiting_job(Some((&landed, VIDEO.len() as u64)));

    assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Delivered);

    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")], "a second copy reached the watchfolder");
    let row = r.repo.get_job(job.id).unwrap().unwrap();
    assert_eq!(row.file_path.as_deref(), Some(landed.to_string_lossy().as_ref()));
    assert!(!row.delivery_wait);
    assert!(r.events(job.id).iter().any(|e| e == "Already in the watchfolder from the interrupted delivery; not copied again"));
    assert!(!r.workspace(job.id).exists());
}

/// A file of another size at the marker is not this job's: it stays, and the
/// video is delivered next to it, with the warning start-up recovery writes.
#[tokio::test]
async fn a_file_of_another_size_at_the_marker_is_left_and_the_video_delivered_next_to_it() {
    let r = rig(false, false);
    std::fs::create_dir_all(&r.watch).unwrap();
    let other = r.watch.join(format!("{SLUG}.mxf"));
    std::fs::write(&other, b"someone else's file").unwrap();
    let job = r.waiting_job(Some((&other, VIDEO.len() as u64)));

    assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Delivered);

    assert_eq!(std::fs::read(&other).unwrap(), b"someone else's file", "a file in the watchfolder was touched");
    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf"), format!("{SLUG}_2.mxf")]);
    assert_eq!(std::fs::read(r.watch.join(format!("{SLUG}_2.mxf"))).unwrap(), VIDEO);
    assert!(r.events(job.id).iter().any(|e| e.contains("it is not treated as this job's video")));
}

/// Test 6: the waiting file changed (same size) or is gone: nothing is
/// delivered from it, its workspace is removed, and the job runs again.
#[tokio::test]
async fn a_waiting_video_that_changed_or_is_gone_is_never_delivered() {
    for gone in [false, true] {
        let r = rig(false, false);
        let job = r.waiting_job(None);
        let ready = PathBuf::from(job.ready_path.clone().unwrap());
        if gone {
            std::fs::remove_file(&ready).unwrap();
        } else {
            let mut bytes = VIDEO.to_vec();
            bytes[3] ^= 0xFF;
            std::fs::write(&ready, bytes).unwrap();
        }

        assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Rerun, "gone: {gone}");

        assert!(r.watch_files().is_empty(), "delivered from a bad file: {:?}", r.watch_files());
        assert!(!r.workspace(job.id).exists(), "the stale workspace is removed");
        let events = r.events(job.id);
        assert!(events.iter().any(|e| e.starts_with("The parked video is missing or changed; running the job again")), "{events:?}");
    }
}

/// Step 7d: the share is still down: the video waits again, as it is, and
/// nothing is fingerprinted, removed or delivered.
#[tokio::test]
async fn a_share_still_down_keeps_the_video_waiting() {
    let r = rig(false, true);
    let job = r.waiting_job(None);
    let ready = PathBuf::from(job.ready_path.clone().unwrap());

    let err = r.engine.deliver_ready(&job, OWNER, None).await.unwrap_err();

    assert!(format!("{err:#}").contains("DELIVERY_FAILED"), "{err:#}");
    assert_eq!(std::fs::read(&ready).unwrap(), VIDEO, "the waiting video was touched");
    let row = r.repo.get_job(job.id).unwrap().unwrap();
    assert!(row.delivery_wait);
    assert_eq!(row.ready_path.as_deref(), Some(ready.to_string_lossy().as_ref()));
    assert!(!r.engine.watchfolder_state().reachable(), "the lease gate closes at once");
    assert!(r.events(job.id).iter().any(|e| e.starts_with("Watchfolder not reachable; the finished video waits")));

    // Back: delivered from the same file.
    r.unblock();
    assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Delivered);
    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")]);
}

/// B2 and B3 of P11.2a, through the engine: each wait in a row backs off
/// further (30 s, 60 s, 2 min, 5 min), the WARN line and the audit row are
/// written once for the same cause and again when the cause changes, and a
/// delivery ends the series.
#[tokio::test]
async fn repeated_waits_back_off_and_say_the_same_cause_once() {
    let r = rig(false, true);
    let job = r.waiting_job(None);
    let id = job.id;
    let lines = |r: &Rig| r.events(id).into_iter().filter(|e| e.starts_with("Watchfolder not reachable")).count();
    let audits = |r: &Rig| r.repo.get_recent_audit_logs(1000).unwrap().len();
    let audits_before = audits(&r);
    let mut job = job;
    for (n, backoff) in [30, 60, 120, 300, 300].into_iter().enumerate() {
        r.engine.deliver_ready(&job, OWNER, None).await.unwrap_err();
        assert_eq!(r.engine.wait_backoff(id).num_seconds(), backoff, "wait {}", n + 1);
        // What the worker does with it.
        let line = r.engine.wait_line(id).expect("the wait's line");
        assert!(r.repo.requeue_waiting_delivery(id, OWNER, Duration::zero(), &line).unwrap());
        job = r.repo.lease_job_when(OWNER, 3600, true).unwrap().unwrap();
    }
    assert_eq!(lines(&r), 1, "{:?}", r.events(id));
    assert_eq!(audits(&r) - audits_before, 1);

    // Another cause (the earlier rename's file cannot be looked at): said once more.
    let unreadable = r.watch.join("x.mxf");
    assert!(r.repo.mark_delivering(id, OWNER, &unreadable.to_string_lossy(), 1).unwrap());
    r.engine.deliver_ready(&job, OWNER, None).await.unwrap_err();
    assert_eq!(lines(&r), 2, "a new cause is written");
    assert_eq!(audits(&r) - audits_before, 2);
    assert_eq!(r.engine.wait_backoff(id).num_seconds(), 300);

    // Delivered (the earlier rename is found there): the series ends.
    r.unblock();
    std::fs::create_dir_all(&r.watch).unwrap();
    std::fs::write(r.watch.join("x.mxf"), b"x").unwrap();
    assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Delivered);
    assert_eq!((r.engine.wait_backoff(id).num_seconds(), r.engine.wait_line(id)), (30, None));
}

/// A marker the share cannot answer about: the rename may have landed, so
/// nothing is copied; the video waits again, and the lease gate closes.
#[tokio::test]
async fn a_marker_the_share_cannot_answer_about_waits_again_without_copying() {
    let r = rig(false, true);
    let unreadable = r.watch.join(format!("{SLUG}.mxf"));
    let job = r.waiting_job(Some((&unreadable, VIDEO.len() as u64)));

    let err = r.engine.deliver_ready(&job, OWNER, None).await.unwrap_err();

    assert!(format!("{err:#}").contains("DELIVERY_FAILED"), "{err:#}");
    assert!(err.downcast_ref::<omni_broadcast::pipeline::DeliveryNeedsCheck>().is_none(), "a share that cannot say waits");
    assert!(r.watch_files().is_empty(), "copied although the earlier rename may have landed");
    assert!(r.repo.get_job(job.id).unwrap().unwrap().delivery_wait);
    assert!(r.workspace(job.id).join(format!("{}_output.mxf", job.id)).exists());
    assert!(!r.engine.watchfolder_state().reachable());
}

/// Finding 7: the marker is in a folder that is not the watchfolder now (the
/// setting changed while the video waited): a person decides, nothing is
/// copied, the old folder is not asked, and the lease gate stays open for the
/// other waiting videos.
#[tokio::test]
async fn a_marker_of_an_older_watchfolder_goes_to_a_person_and_keeps_the_gate_open() {
    let r = rig(false, false);
    let old = r.root.join("old-share").join(format!("{SLUG}.mxf"));
    let job = r.waiting_job(Some((&old, VIDEO.len() as u64)));

    let err = r.engine.deliver_ready(&job, OWNER, None).await.unwrap_err();

    let check = err.downcast_ref::<omni_broadcast::pipeline::DeliveryNeedsCheck>().expect("a person decides");
    assert_eq!(check.code, omni_broadcast::errors::ErrorCode::DeliveryUnconfirmed);
    assert_eq!(
        check.message,
        format!("The watchfolder changed while {SLUG}.mxf was waiting; check {} and Dalet before retrying", old.parent().unwrap().display())
    );
    assert!(r.watch_files().is_empty());
    assert!(r.engine.watchfolder_state().reachable(), "the gate closed for a folder that is not the watchfolder");
    assert!(r.workspace(job.id).join(format!("{}_output.mxf", job.id)).exists());
}

/// Finding 2, at the engine: a rename was attempted (marker) and its file is
/// not in the watchfolder now: nothing is copied, a person decides.
#[tokio::test]
async fn a_marker_whose_file_is_not_there_is_never_delivered_again_on_its_own() {
    let r = rig(false, false);
    std::fs::create_dir_all(&r.watch).unwrap();
    let gone = r.watch.join(format!("{SLUG}.mxf"));
    let job = r.waiting_job(Some((&gone, VIDEO.len() as u64)));

    let err = r.engine.deliver_ready(&job, OWNER, None).await.unwrap_err();

    assert!(r.watch_files().is_empty(), "delivered again on its own: {:?}", r.watch_files());
    let check = err.downcast_ref::<omni_broadcast::pipeline::DeliveryNeedsCheck>().expect("a person decides");
    assert_eq!(check.code, omni_broadcast::errors::ErrorCode::DeliveryUnconfirmed);
    assert_eq!(
        check.message,
        format!("The delivery of {SLUG}.mxf was interrupted and the file is not in the watchfolder now. Check Dalet for {SLUG}.mxf before retrying")
    );
    assert!(r.workspace(job.id).join(format!("{}_output.mxf", job.id)).exists());
}

/// Finding 4: the attempt the share cut left its fresh `.slug.mxf.tmp`; the
/// delivery after the wait reclaims it and lands under the slug itself.
#[tokio::test]
async fn the_leftover_staging_file_of_the_cut_attempt_does_not_push_the_video_to_2() {
    let r = rig(false, false);
    std::fs::create_dir_all(&r.watch).unwrap();
    let leftover = r.watch.join(format!(".{SLUG}.mxf.tmp"));
    std::fs::write(&leftover, &VIDEO[..10]).unwrap();
    let job = r.waiting_job(None);

    assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Delivered);

    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")], "the leftover stayed or the video went to _2");
    assert!(!r.events(job.id).iter().any(|e| e.starts_with("Slug collision")), "{:?}", r.events(job.id));
}

/// P11.2b review finding 2: the video parked at T0; a later delivery attempt
/// after the wait was cut mid-copy too and left a fresh `.slug.mxf.tmp`, newer
/// than the park. The next try reclaims it (it was there before that try
/// began) and lands under the slug itself, with no collision warning.
#[tokio::test]
async fn a_staging_file_cut_by_a_later_attempt_does_not_push_the_video_to_2() {
    let r = rig(false, false);
    std::fs::create_dir_all(&r.watch).unwrap();
    let job = r.waiting_job(None);
    let parked = job.ready_at.expect("the park time was not recorded");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let cut = r.watch.join(format!(".{SLUG}.mxf.tmp"));
    std::fs::write(&cut, &VIDEO[..7]).unwrap();
    let touched = chrono::DateTime::<chrono::Utc>::from(std::fs::metadata(&cut).unwrap().modified().unwrap());
    assert!(touched > parked, "the cut copy must be newer than the first park");
    std::thread::sleep(std::time::Duration::from_millis(20));

    assert_eq!(r.engine.deliver_ready(&job, OWNER, None).await.unwrap(), ReadyOutcome::Delivered);

    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")], "the leftover stayed or the video went to _2");
    assert_eq!(std::fs::read(r.watch.join(format!("{SLUG}.mxf"))).unwrap(), VIDEO);
    assert!(!r.events(job.id).iter().any(|e| e.starts_with("Slug collision")), "{:?}", r.events(job.id));
}

/// P11.2b M2: the cap on a waiting video that cannot be read counts from its
/// first park; an unknown park time is never past it.
#[test]
fn the_unreadable_wait_is_capped_at_two_hours_from_the_first_park() {
    use omni_broadcast::pipeline::{unreadable_too_long, UNREADABLE_WAIT_LIMIT};
    let now = chrono::Utc::now();
    assert_eq!(UNREADABLE_WAIT_LIMIT, Duration::hours(2));
    assert!(!unreadable_too_long(Some(now - Duration::minutes(119)), now));
    assert!(!unreadable_too_long(Some(now - Duration::hours(2)), now));
    assert!(unreadable_too_long(Some(now - Duration::minutes(121)), now));
    assert!(!unreadable_too_long(None, now));
}

/// Finding 5: once the daemon is stopping, a waiting video goes straight back
/// to waiting: nothing copied, no event, and well inside the stop's budget.
#[tokio::test]
async fn a_stopping_engine_sends_a_waiting_video_back_at_once() {
    let r = rig(false, false);
    let job = r.waiting_job(None);
    let events = r.events(job.id).len();
    r.engine.stop_new_deliveries();

    let started = std::time::Instant::now();
    let err = tokio::time::timeout(std::time::Duration::from_secs(5), r.engine.deliver_ready(&job, OWNER, None))
        .await
        .expect("returned at once, not parked on the stop")
        .unwrap_err();

    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert!(format!("{err:#}").contains("DELIVERY_FAILED"), "{err:#}");
    assert!(r.watch_files().is_empty());
    assert_eq!(r.events(job.id).len(), events, "a stop writes nothing on the job");
    assert!(r.repo.get_job(job.id).unwrap().unwrap().delivery_wait);
    assert!(r.engine.watchfolder_state().reachable(), "a stop is not a share that is down");
}

/// Finding 6: the waiting file cannot be opened now (held open elsewhere),
/// but it is there: wait, never delete the verified video.
#[cfg(windows)]
#[tokio::test]
async fn a_waiting_video_that_cannot_be_opened_now_waits_and_is_kept() {
    use std::os::windows::fs::OpenOptionsExt;
    let r = rig(false, false);
    let job = r.waiting_job(None);
    let ready = PathBuf::from(job.ready_path.clone().unwrap());
    let held_open = std::fs::OpenOptions::new().read(true).share_mode(0).open(&ready).unwrap();

    let err = r.engine.deliver_ready(&job, OWNER, None).await.unwrap_err();
    drop(held_open);

    assert!(format!("{err:#}").contains("DELIVERY_FAILED"), "{err:#}");
    assert_eq!(std::fs::read(&ready).unwrap(), VIDEO, "the verified video was removed");
    assert!(r.repo.get_job(job.id).unwrap().unwrap().delivery_wait);
    assert!(r.events(job.id).iter().any(|e| e.contains("the waiting video cannot be read now")), "{:?}", r.events(job.id));
    assert!(r.watch_files().is_empty());
}

/// Risk (a) on the held path: an approved held video that waited, whose
/// earlier rename landed after all, is finished with that file by the held
/// path's own end, and nothing is copied again.
#[tokio::test]
async fn an_approved_held_video_whose_rename_landed_is_not_copied_again() {
    let r = rig(false, false);
    std::fs::create_dir_all(&r.watch).unwrap();
    let id = r
        .repo
        .enqueue(&NewJob::new("https://news.example.invalid/story/2", SLUG, "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    assert_eq!(r.repo.lease_job(OWNER, 3600).unwrap().unwrap().id, id);
    let held_dir = r.root.join("temp").join("held").join(id.to_string());
    std::fs::create_dir_all(&held_dir).unwrap();
    let held_file = held_dir.join(format!("{SLUG}.mxf"));
    std::fs::write(&held_file, VIDEO).unwrap();
    let sha = omni_core::held::sha256_file(&held_file).unwrap();
    let until = chrono::Utc::now() + Duration::hours(24);
    let held_path = held_file.to_string_lossy().into_owned();
    assert!(r.repo.hold_output(id, OWNER, &held_path, VIDEO.len() as i64, &sha, Some(3.0), until).unwrap());
    assert!(r.repo.finish(id, OWNER, omni_core::models::JobStatus::AwaitingApproval, None, None, None).unwrap());
    assert!(r.repo.approve_held(id, &sha, "desk").unwrap());
    // The approved delivery's attempt: marked, the rename "failed", parked.
    assert_eq!(r.repo.lease_approved_held_job(OWNER, 3600).unwrap().unwrap().id, id);
    let landed = r.watch.join(format!("{SLUG}.mxf"));
    assert!(r.repo.mark_delivering(id, OWNER, &landed.to_string_lossy(), VIDEO.len() as u64).unwrap());
    assert!(r.repo.park_for_delivery(id, OWNER, &held_path, VIDEO.len() as u64, &sha, 3.0).unwrap());
    assert!(r.repo.requeue_waiting_delivery(id, OWNER, Duration::zero(), "waits").unwrap());
    std::fs::write(&landed, VIDEO).unwrap();

    let job = r.repo.lease_approved_held_job_when(OWNER, 3600, true).unwrap().expect("the waiting held job");
    let held = r.repo.approved_held(id).unwrap().expect("still approved");
    r.engine.deliver_held(&job, OWNER, &held, None).await.expect("finished from the marker");

    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")], "a second copy reached the watchfolder");
    assert!(r.repo.finish_held_delivery(id, OWNER).unwrap());
    let row = r.repo.get_job(id).unwrap().unwrap();
    assert_eq!(row.status, omni_core::models::JobStatus::Completed);
    assert_eq!(row.file_path.as_deref(), Some(landed.to_string_lossy().as_ref()));
    assert!(!row.delivery_wait);
    assert_eq!(r.repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("delivered"));
}

/// Every file name under `dir`, recursively.
fn walk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out
}
