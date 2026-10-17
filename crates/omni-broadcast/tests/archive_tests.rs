//! The delivered-source archive (plan P1.7), through the engine's
//! post-delivery step. No ffmpeg needed: the step only moves files and
//! writes to the database.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use omni_broadcast::pipeline::BroadcastEngine;
use omni_core::archive::{day_dir, ArchiveSettings};
use omni_core::config::ArchiveConfig;
use omni_core::models::NewJob;
use omni_core::paths::AppPaths;
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const OWNER: &str = "test:1:1";
const SLUG: &str = "1_DIMITRIOU_LIMANI";

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: Repository,
    engine: BroadcastEngine,
    id: i64,
}

/// A leased (RUNNING) job with its downloaded source in `temp/jobs/{id}/`.
fn rig(archive: Option<ArchiveSettings>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    std::fs::create_dir_all(root.join("watchfolder")).unwrap();
    let engine = BroadcastEngine::new(
        repo.clone(),
        root.join("no-yt-dlp.exe"),
        root.join("no-ffmpeg.exe"),
        root.join("no-ffprobe.exe"),
        root.join("no-bmx.exe"),
        root.join("temp"),
        root.join("watchfolder"),
    )
    .with_archive(archive);
    let mut job = NewJob::new("https://media.example.gr/v/clip.mp4?token=SECRET123&sig=abc", SLUG, "DIMITRIOU");
    job.keyword = "LIMANI".into();
    let id = repo.enqueue(&job, DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    let leased = repo.lease_job(OWNER, 180).unwrap().expect("job is leasable");
    assert_eq!(leased.id, id);
    repo.set_compliance_report(id, r#"{"pass":true,"checks":[]}"#).unwrap();
    Rig { _dir: dir, root, repo, engine, id }
}

fn job_temp(r: &Rig) -> PathBuf {
    r.root.join("temp").join("jobs").join(r.id.to_string())
}

fn source_in_job_temp(r: &Rig) -> PathBuf {
    let dir = job_temp(r);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("scratch.tmp"), b"scratch").unwrap();
    let src = dir.join("download.mp4");
    std::fs::write(&src, b"downloaded source bytes").unwrap();
    src
}

fn files_under(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p);
            }
        }
    }
    out
}

fn today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

#[tokio::test]
async fn a_delivered_source_is_in_the_archive_with_its_sidecar_and_the_workspace_is_gone() {
    let archive = tempfile::tempdir().unwrap();
    let r = rig(Some(ArchiveSettings::new(archive.path().to_path_buf(), 14)));
    let src = source_in_job_temp(&r);
    // A delivered file already in the watchfolder must come through untouched.
    std::fs::write(r.root.join("watchfolder").join("1_DIMITRIOU_LIMANI.mxf"), b"on air").unwrap();
    let watch_before = files_under(&r.root.join("watchfolder"));

    r.engine.archive_then_clean(r.id, OWNER, Some(&src), &job_temp(&r), "1_DIMITRIOU_LIMANI.mxf").await;

    let dir = day_dir(archive.path(), today());
    let archived = dir.join(format!("{}_{SLUG}.mp4", r.id));
    assert_eq!(std::fs::read(&archived).unwrap(), b"downloaded source bytes");
    let side: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{}_{SLUG}.json", r.id))).unwrap()).unwrap();
    assert_eq!(side["job_id"], r.id);
    assert_eq!(side["delivered_filename"], "1_DIMITRIOU_LIMANI.mxf");
    assert_eq!(side["compliance"]["pass"], true);
    assert_eq!(side["source_address"], "media.example.gr/v/clip.mp4");
    assert!(!job_temp(&r).exists(), "the workspace must be removed after archiving");
    assert_eq!(files_under(&r.root.join("watchfolder")), watch_before, "the archive must not touch the watchfolder");
    assert_eq!(r.repo.get_job(r.id).unwrap().unwrap().stage.as_str(), "ARCHIVE");
}

#[tokio::test]
async fn with_the_archive_off_the_source_is_deleted_with_the_workspace_as_before() {
    // A real, empty archive root, configured off.
    let archive = tempfile::tempdir().unwrap();
    let cfg = ArchiveConfig { enabled: false, retention_days: 14, path: archive.path().to_string_lossy().into_owned() };
    let paths = AppPaths::with_root(archive.path(), "config.json");
    let settings = ArchiveSettings::from_config(&cfg, &paths, &archive.path().join("..").join("no-watchfolder"));
    assert!(!settings.writes());
    let r = rig(Some(settings));
    let src = source_in_job_temp(&r);

    r.engine.archive_then_clean(r.id, OWNER, Some(&src), &job_temp(&r), "x.mxf").await;

    assert!(!job_temp(&r).exists());
    assert!(files_under(archive.path()).is_empty() && std::fs::read_dir(archive.path()).unwrap().count() == 0, "the root must stay empty");
    assert_ne!(r.repo.get_job(r.id).unwrap().unwrap().stage.as_str(), "ARCHIVE");
}

#[tokio::test]
async fn an_archive_that_cannot_be_written_never_fails_the_job() {
    let dir = tempfile::tempdir().unwrap();
    // The archive root is a file: nothing can be created under it.
    let blocked = dir.path().join("archive");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let r = rig(Some(ArchiveSettings::new(blocked.clone(), 14)));
    let src = source_in_job_temp(&r);

    r.engine.archive_then_clean(r.id, OWNER, Some(&src), &job_temp(&r), "x.mxf").await;

    assert!(!job_temp(&r).exists(), "cleanup must still run");
    assert_eq!(std::fs::read(&blocked).unwrap(), b"not a directory");
    let events = r.repo.get_job_events(r.id, 50).unwrap();
    assert!(
        events.iter().any(|e| e.level == "WARN" && e.message.starts_with("ARCHIVE_FAILED")),
        "no ARCHIVE_FAILED event: {events:?}"
    );
    // The failure leaves the job row to the worker: still RUNNING.
    assert_eq!(r.repo.get_job(r.id).unwrap().unwrap().status.as_str(), "RUNNING");
}

#[tokio::test]
async fn an_attachment_is_copied_and_stays_in_the_attachment_store() {
    let archive = tempfile::tempdir().unwrap();
    let r = rig(Some(ArchiveSettings::new(archive.path().to_path_buf(), 14)));
    let store = r.root.join("temp").join("attachments").join(r.id.to_string());
    std::fs::create_dir_all(&store).unwrap();
    let attached = store.join("clip.mov");
    std::fs::write(&attached, b"attached bytes").unwrap();
    std::fs::create_dir_all(job_temp(&r)).unwrap();

    r.engine.archive_then_clean(r.id, OWNER, Some(&attached), &job_temp(&r), "x.mxf").await;

    assert_eq!(std::fs::read(&attached).unwrap(), b"attached bytes", "the store file was removed or changed");
    let copy = day_dir(archive.path(), today()).join(format!("{}_{SLUG}.mov", r.id));
    assert_eq!(std::fs::read(copy).unwrap(), b"attached bytes");
}

#[tokio::test]
async fn the_sidecar_holds_no_full_address_and_no_cookie() {
    let archive = tempfile::tempdir().unwrap();
    let r = rig(Some(ArchiveSettings::new(archive.path().to_path_buf(), 14)));
    r.repo
        .set_compliance_report(
            r.id,
            r#"{"pass":true,"note":"from https://cdn.example.gr/a.mp4?sig=SECRET123","cookie":"sid=SECRET123"}"#,
        )
        .unwrap();
    let src = source_in_job_temp(&r);

    r.engine.archive_then_clean(r.id, OWNER, Some(&src), &job_temp(&r), "x.mxf").await;

    let text = std::fs::read_to_string(day_dir(archive.path(), today()).join(format!("{}_{SLUG}.json", r.id))).unwrap();
    assert!(!text.contains("SECRET123"), "{text}");
    assert!(!text.contains("https://") && !text.contains("http://"), "{text}");
    assert!(!text.contains("sid="), "{text}");
    assert!(text.contains("media.example.gr/v/clip.mp4"));
}

#[tokio::test]
async fn a_lost_lease_writes_no_archive() {
    let archive = tempfile::tempdir().unwrap();
    let r = rig(Some(ArchiveSettings::new(archive.path().to_path_buf(), 14)));
    let src = source_in_job_temp(&r);

    r.engine.archive_then_clean(r.id, "someone-else:9:9", Some(&src), &job_temp(&r), "x.mxf").await;

    assert!(files_under(archive.path()).is_empty(), "an archive was written without the lease");
    assert!(r.repo.get_job_events(r.id, 50).unwrap().iter().all(|e| !e.message.contains("ARCHIVE")));
}

#[tokio::test]
async fn an_archive_overlapping_the_watchfolder_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let watch = dir.path().join("watchfolder");
    std::fs::create_dir_all(&watch).unwrap();
    let cfg = ArchiveConfig { enabled: true, retention_days: 14, path: watch.join("archive").to_string_lossy().into_owned() };
    let paths = AppPaths::with_root(dir.path(), "config.json");
    let settings = ArchiveSettings::from_config(&cfg, &paths, &watch);
    assert!(settings.conflict);
    // The rig's own watchfolder is `{root}/watchfolder`, a different tempdir.
    let r = rig(Some(settings));
    let src = source_in_job_temp(&r);

    r.engine.archive_then_clean(r.id, OWNER, Some(&src), &job_temp(&r), "x.mxf").await;

    assert!(files_under(&watch).is_empty(), "something was written into the watchfolder");
    assert!(!job_temp(&r).exists(), "cleanup still runs");
}
