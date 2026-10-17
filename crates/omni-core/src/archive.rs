//! The delivered-source archive (plan P1.7).
//!
//! After a job's MXF is in the watchfolder its downloaded source is kept for
//! `archive.retention_days` under `{root}/YYYY/MM/DD/{job_id}_{slug}.{ext}`,
//! with a JSON sidecar beside it. The day is the *local* date of delivery:
//! it is the newsroom's day, and the nightly 04:00 retention task counts in
//! the same local days.
//!
//! The archive only ever writes inside its own root. It never touches the
//! watchfolder, and everything that deletes does so by whole day directory
//! whose name parses as a date, never by filename prefix and never through a
//! link.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Datelike, Duration, NaiveDate};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::ArchiveConfig;
use crate::models::Job;
use crate::paths::AppPaths;

/// Where and for how long, resolved against the install directory.
#[derive(Debug, Clone)]
pub struct ArchiveSettings {
    pub root: PathBuf,
    pub retention_days: i64,
    /// `archive.enabled`: whether new sources are kept. Retention and the
    /// disk guard still run on `root` when this is off, so old archives expire.
    pub enabled: bool,
    /// The archive and the watchfolder contain one another. Nothing is
    /// written, purged or deleted then: the archive must never be able to
    /// reach a file Dalet owns.
    pub conflict: bool,
}

impl ArchiveSettings {
    pub fn new(root: PathBuf, retention_days: i64) -> Self {
        Self { root, retention_days: retention_days.max(1), enabled: true, conflict: false }
    }

    /// Resolve the configured archive against the install directory and
    /// check it against the watchfolder.
    pub fn from_config(cfg: &ArchiveConfig, paths: &AppPaths, watchfolder: &Path) -> Self {
        let root = paths.resolve(&cfg.path);
        let conflict = overlaps(&root, watchfolder);
        Self { root, retention_days: cfg.retention_days.max(1), enabled: cfg.enabled, conflict }
    }

    /// New sources are kept.
    pub fn writes(&self) -> bool {
        self.enabled && !self.conflict
    }

    /// Retention and the disk guard may delete in `root`.
    pub fn purges(&self) -> bool {
        !self.conflict
    }
}

/// `path` made comparable: the nearest existing ancestor canonicalized, the
/// rest appended with `.` and `..` resolved, and lower-cased on Windows
/// (where paths are case-insensitive).
fn normalise(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut base = path.to_path_buf();
    while !base.exists() {
        match base.file_name() {
            Some(name) => rest.push(name.to_os_string()),
            None => break,
        }
        if !base.pop() {
            break;
        }
    }
    let mut out = base.canonicalize().unwrap_or(base);
    for part in rest.into_iter().rev() {
        if part == ".." {
            out.pop();
        } else if part != "." {
            out.push(part);
        }
    }
    if cfg!(windows) {
        PathBuf::from(out.to_string_lossy().to_lowercase())
    } else {
        out
    }
}

/// Whether one of the two directories is, or lies inside, the other.
pub fn overlaps(a: &Path, b: &Path) -> bool {
    let (a, b) = (normalise(a), normalise(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// `{root}/YYYY/MM/DD`.
pub fn day_dir(root: &Path, day: NaiveDate) -> PathBuf {
    root.join(format!("{:04}", day.year())).join(format!("{:02}", day.month())).join(format!("{:02}", day.day()))
}

/// A file name part that is safe on every filesystem.
fn clean(part: &str) -> String {
    part.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// Host and path only: a media address often carries a token in its query.
pub fn addr_for_record(url: &str) -> String {
    let full = if url.starts_with("//") { format!("https:{url}") } else { url.to_string() };
    match reqwest::Url::parse(&full) {
        Ok(u) => format!("{}{}", u.host_str().unwrap_or("?"), u.path()),
        Err(_) => "(not an address)".to_string(),
    }
}

/// `value` with every address cut to host and path, and every key that could
/// hold a secret dropped.
fn scrub(value: Value) -> Value {
    match value {
        Value::String(s) if s.contains("://") || s.starts_with("//") => Value::String(addr_for_record(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(scrub).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| {
                    let k = k.to_ascii_lowercase();
                    !["cookie", "token", "authorization", "secret", "password"].iter().any(|bad| k.contains(bad))
                })
                .map(|(k, v)| (k, scrub(v)))
                .collect(),
        ),
        other => other,
    }
}

fn parsed(json_text: Option<&str>) -> Value {
    json_text.and_then(|t| serde_json::from_str::<Value>(t).ok()).map(scrub).unwrap_or(Value::Null)
}

/// What the sidecar says about a job. No full address, no cookie, no mail
/// sender or notes.
pub fn sidecar(job: &Job, archived_name: &str, source_bytes: u64, delivered_filename: &str, day: NaiveDate) -> Value {
    json!({
        "version": 1,
        "job_id": job.id,
        "slug": job.slug,
        "index": job.index_str,
        "journalist": job.journalist,
        "keyword": job.keyword,
        "source_address": addr_for_record(&job.url),
        "extraction_method": job.extraction_method,
        "media_format": job.media_format,
        "duration_secs": job.duration_secs,
        "delivered_filename": delivered_filename,
        "delivered_on": day.format("%Y-%m-%d").to_string(),
        "archived_file": archived_name,
        "archived_bytes": source_bytes,
        "compliance": parsed(job.compliance_json.as_deref()),
        "candidates": parsed(job.candidates_json.as_deref()),
        "stage_timings": parsed(job.stage_timings_json.as_deref()),
    })
}

/// Copy `from` to `to`, flushed to disk. It is written under `{to}.tmp` and
/// renamed, so nobody ever sees a partial `to`; a failure removes the partial.
fn copy_durably(from: &Path, to: &Path) -> Result<u64> {
    let mut src = fs::File::open(from).with_context(|| format!("cannot read {from:?}"))?;
    copy_reader_durably(&mut src, to)
}

fn copy_reader_durably(src: &mut impl std::io::Read, to: &Path) -> Result<u64> {
    let mut tmp_name = to.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    // A temp file left by a crash is ours (our name, inside the archive root).
    let _ = fs::remove_file(&tmp);
    let result = (|| -> Result<u64> {
        let mut dst = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("cannot create {tmp:?}"))?;
        let n = std::io::copy(src, &mut dst)?;
        dst.sync_all()?;
        drop(dst);
        fs::rename(&tmp, to).with_context(|| format!("cannot move {tmp:?} to {to:?}"))?;
        Ok(n)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Keep `source` in the archive under `day`, with its sidecar.
///
/// A file inside `job_temp` is moved (renamed; copied and removed when the
/// archive is on another volume). Any other file (an email attachment, an MCR
/// upload) is copied: the attachment store still owns it. An existing archive
/// file is never overwritten; the name gets `_N`.
///
/// Returns the archived file's path.
pub fn archive_source(
    root: &Path,
    job: &Job,
    source: &Path,
    job_temp: &Path,
    delivered_filename: &str,
    day: NaiveDate,
) -> Result<PathBuf> {
    let dir = day_dir(root, day);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {dir:?}"))?;

    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .map(clean)
        .filter(|e| !e.is_empty() && e.len() <= 8)
        .unwrap_or_else(|| "bin".to_string());
    let stem = format!("{}_{}", job.id, clean(&job.slug));
    let mut n = 0u32;
    let (stem, dest) = loop {
        let candidate = if n == 0 { stem.clone() } else { format!("{stem}_{n}") };
        let dest = dir.join(format!("{candidate}.{ext}"));
        if !dest.exists() && !dir.join(format!("{candidate}.json")).exists() {
            break (candidate, dest);
        }
        n += 1;
        anyhow::ensure!(n < 1000, "no free archive name for job {}", job.id);
    };

    let owned = source.starts_with(job_temp);
    let bytes = fs::metadata(source).with_context(|| format!("cannot read {source:?}"))?.len();
    if owned {
        if fs::rename(source, &dest).is_err() {
            copy_durably(source, &dest)?;
            fs::remove_file(source).with_context(|| format!("copied {source:?} but could not remove it"))?;
        }
    } else {
        copy_durably(source, &dest)?;
    }

    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    let doc = sidecar(job, &name, bytes, delivered_filename, day);
    let json_path = dir.join(format!("{stem}.json"));
    let tmp = dir.join(format!("{stem}.json.tmp"));
    let write = || -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string_pretty(&doc)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &json_path)?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(e.context(format!("the source is archived at {dest:?} but its sidecar could not be written")));
    }
    Ok(dest)
}

fn is_real_dir(entry: &fs::DirEntry) -> bool {
    entry.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false)
}

/// The numeric value of a directory name of exactly `digits` ASCII digits.
fn numeric_name(entry: &fs::DirEntry, digits: usize) -> Option<u32> {
    let name = entry.file_name().to_string_lossy().to_string();
    (name.len() == digits && name.bytes().all(|b| b.is_ascii_digit())).then(|| name.parse().ok()).flatten()
}

/// Every `YYYY/MM/DD` directory under `root` whose names form a real date,
/// oldest first. Links, files and anything not shaped like a date are left out.
pub fn day_dirs(root: &Path) -> Vec<(NaiveDate, PathBuf)> {
    let mut out = Vec::new();
    let Ok(years) = fs::read_dir(root) else { return out };
    for y in years.flatten().filter(is_real_dir) {
        let Some(year) = numeric_name(&y, 4) else { continue };
        let Ok(months) = fs::read_dir(y.path()) else { continue };
        for m in months.flatten().filter(is_real_dir) {
            let Some(month) = numeric_name(&m, 2) else { continue };
            let Ok(days) = fs::read_dir(m.path()) else { continue };
            for d in days.flatten().filter(is_real_dir) {
                let Some(day) = numeric_name(&d, 2) else { continue };
                if let Some(date) = NaiveDate::from_ymd_opt(year as i32, month, day) {
                    out.push((date, d.path()));
                }
            }
        }
    }
    out.sort();
    out
}

/// Remove month and year directories left empty (non-recursive: a directory
/// with anything in it stays).
fn prune_empty(root: &Path) {
    let Ok(years) = fs::read_dir(root) else { return };
    for y in years.flatten().filter(is_real_dir) {
        if numeric_name(&y, 4).is_none() {
            continue;
        }
        if let Ok(months) = fs::read_dir(y.path()) {
            for m in months.flatten().filter(is_real_dir) {
                if numeric_name(&m, 2).is_some() {
                    let _ = fs::remove_dir(m.path());
                }
            }
        }
        let _ = fs::remove_dir(y.path());
    }
}

fn remove_day(path: &Path) -> bool {
    match fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(e) => {
            warn!("Archive: could not remove {path:?}: {e}");
            false
        }
    }
}

/// Nightly retention: delete the day directories older than `retention_days`
/// (by the date in their name, never by modification time), then the empty
/// month and year directories. Returns the number of days removed.
pub fn purge_expired(root: &Path, today: NaiveDate, retention_days: i64) -> usize {
    let cutoff = today - Duration::days(retention_days.max(1));
    let mut removed = 0;
    for (date, path) in day_dirs(root) {
        if date < cutoff && remove_day(&path) {
            removed += 1;
        }
    }
    prune_empty(root);
    if removed > 0 {
        info!("Archive: removed {removed} expired day(s)");
    }
    removed
}

/// Low disk: delete the oldest day directories one at a time until
/// `enough()` says there is room again, or the archive is empty. Returns the
/// number of days removed.
pub fn purge_oldest_until(root: &Path, mut enough: impl FnMut() -> bool) -> usize {
    let mut removed = 0;
    for (date, path) in day_dirs(root) {
        if enough() {
            break;
        }
        if remove_day(&path) {
            removed += 1;
            warn!("Archive: removed {date} early to free disk space");
        }
    }
    prune_empty(root);
    removed
}

/// Whether the archive and temp directories share a volume, so deleting
/// archive days can help. Same drive or share, and the same size.
pub fn same_volume(archive: &Path, temp: &Path) -> bool {
    let key = |p: &Path| {
        let mut probe = p.to_path_buf();
        while !probe.exists() {
            if !probe.pop() {
                return None;
            }
        }
        let canon = probe.canonicalize().ok()?;
        let prefix = canon.components().next()?.as_os_str().to_ascii_lowercase();
        let total = crate::selftest::disk_space(&probe)?.1;
        Some((prefix, total))
    };
    match (key(archive), key(temp)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The daemon's low-disk step: purge the oldest archive days, one at a time,
/// while the archive's own volume has less than `threshold_bytes` free, or
/// (when `temp_free` is given, because the archive shares temp's volume) while
/// the temp volume does. Returns the number of days removed.
pub fn relieve_low_disk(
    root: &Path,
    threshold_bytes: u64,
    archive_free: impl Fn() -> Option<u64>,
    temp_free: Option<&dyn Fn() -> Option<u64>>,
) -> usize {
    let below = |free: Option<u64>| free.is_some_and(|f| f < threshold_bytes);
    let low = || below(archive_free()) || temp_free.is_some_and(|f| below(f()));
    if !low() || !root.is_dir() {
        return 0;
    }
    purge_oldest_until(root, || !low())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn put(root: &Path, date: NaiveDate, name: &str) -> PathBuf {
        let dir = day_dir(root, date);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn retention_removes_old_days_and_empty_parents_but_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        let today = day(2026, 10, 17);
        let old = put(&root, today - Duration::days(20), "1_A.mp4");
        let just_over = put(&root, today - Duration::days(15), "2_A.mp4");
        let edge = put(&root, today - Duration::days(14), "3_A.mp4");
        let young = put(&root, today - Duration::days(13), "4_A.mp4");
        let now = put(&root, today, "5_A.mp4");
        // An old year that empties out completely.
        let ancient = put(&root, day(2024, 2, 29), "6_A.mp4");
        // Not dates: kept, with everything in them.
        let notes = root.join("notes");
        fs::create_dir_all(&notes).unwrap();
        fs::write(notes.join("keep.txt"), b"x").unwrap();
        let bad_day = root.join("2020").join("13").join("40");
        fs::create_dir_all(&bad_day).unwrap();
        fs::write(bad_day.join("keep.txt"), b"x").unwrap();
        fs::write(root.join("loose.txt"), b"x").unwrap();
        // Outside the root entirely.
        let outside = tmp.path().join("2000").join("01").join("01");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"x").unwrap();

        assert_eq!(purge_expired(&root, today, 14), 3);

        for gone in [&old, &just_over, &ancient] {
            assert!(!gone.exists(), "{gone:?} should have expired");
        }
        for kept in [&edge, &young, &now, &notes.join("keep.txt"), &bad_day.join("keep.txt"), &root.join("loose.txt"), &outside.join("keep.txt")] {
            assert!(kept.exists(), "{kept:?} must stay");
        }
        assert!(!root.join("2024").exists(), "the emptied year stays behind");
        assert!(!day_dir(&root, today - Duration::days(20)).exists());
    }

    #[test]
    fn the_disk_guard_removes_oldest_days_first_and_stops_when_there_is_room() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        let today = day(2026, 10, 17);
        let dates: Vec<_> = (0..4).map(|i| today - Duration::days(i)).collect();
        for d in &dates {
            put(&root, *d, "1_A.mp4");
        }
        // Each removal frees one unit; the guard wants 2 units free from 0.
        let freed = Cell::new(0u64);
        let removed = purge_oldest_until(&root, || {
            // Count what has gone since the start.
            freed.set(4 - day_dirs(&root).len() as u64);
            freed.get() >= 2
        });
        assert_eq!(removed, 2);
        let left: Vec<_> = day_dirs(&root).into_iter().map(|(d, _)| d).collect();
        assert_eq!(left, vec![dates[1], dates[0]], "the two newest days must survive");
    }

    #[test]
    fn nothing_is_purged_when_space_is_fine_or_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        put(&root, day(2026, 10, 1), "1_A.mp4");
        let fine: &dyn Fn() -> Option<u64> = &|| Some(100);
        assert_eq!(relieve_low_disk(&root, 10, || Some(100), Some(fine)), 0);
        assert_eq!(relieve_low_disk(&root, 10, || None, None), 0);
        assert_eq!(day_dirs(&root).len(), 1);
    }

    #[test]
    fn low_space_on_the_archives_own_volume_purges_it_even_when_temp_is_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        for i in 1..=3 {
            put(&root, day(2026, 10, i), "1_A.mp4");
        }
        // No temp measure: the archive is not on temp's volume. Free reads 0
        // until at least two days are gone.
        let removed = relieve_low_disk(&root, 10, || Some(if day_dirs(&root).len() <= 1 { 100 } else { 0 }), None);
        assert_eq!(removed, 2);
        assert_eq!(day_dirs(&root)[0].0, day(2026, 10, 3));
    }

    #[test]
    fn low_space_on_temp_purges_the_archive_only_when_they_share_the_volume() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        for i in 1..=3 {
            put(&root, day(2026, 10, i), "1_A.mp4");
        }
        let temp_low: &dyn Fn() -> Option<u64> = &|| Some(0);
        // Archive volume fine, temp elsewhere and low: purging cannot help.
        assert_eq!(relieve_low_disk(&root, 10, || Some(100), None), 0);
        assert_eq!(day_dirs(&root).len(), 3);
        // Shared with temp: days go, oldest first, until temp has room (never, here).
        assert_eq!(relieve_low_disk(&root, 10, || Some(100), Some(temp_low)), 3);
    }

    #[test]
    fn an_archive_that_contains_or_sits_inside_the_watchfolder_is_turned_off() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("Watch");
        fs::create_dir_all(&watch).unwrap();
        let cfg = |path: &Path| ArchiveConfig { enabled: true, retention_days: 14, path: path.to_string_lossy().into_owned() };
        let paths = AppPaths::with_root(tmp.path(), "config.json");
        let refused = |s: &ArchiveSettings, why: &str| assert!(s.conflict && !s.writes() && !s.purges(), "{why} must be refused");
        // The archive inside the watchfolder.
        refused(&ArchiveSettings::from_config(&cfg(&watch.join("archive")), &paths, &watch), "inside");
        // The watchfolder inside the archive.
        refused(&ArchiveSettings::from_config(&cfg(tmp.path()), &paths, &watch), "outer");
        // The very same directory, spelled with a `..`.
        refused(&ArchiveSettings::from_config(&cfg(&tmp.path().join("x").join("..").join("Watch")), &paths, &watch), "same");
        if cfg!(windows) {
            refused(&ArchiveSettings::from_config(&cfg(&tmp.path().join("watch").join("a")), &paths, &watch), "case");
        }
        // A sibling stays enabled.
        let sibling = ArchiveSettings::from_config(&cfg(&tmp.path().join("archive")), &paths, &watch);
        assert!(!sibling.conflict && sibling.writes() && sibling.purges());
    }

    #[test]
    fn a_disabled_archive_still_purges_unless_it_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(tmp.path(), "config.json");
        let cfg = ArchiveConfig { enabled: false, retention_days: 14, path: "archive".into() };
        let s = ArchiveSettings::from_config(&cfg, &paths, &tmp.path().join("watch"));
        assert!(!s.writes() && s.purges());
    }

    #[test]
    fn a_stale_temp_file_from_a_crash_does_not_block_the_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("1_A.mp4");
        fs::write(tmp.path().join("1_A.mp4.tmp"), b"half a file from a crash").unwrap();
        assert_eq!(copy_reader_durably(&mut &b"whole"[..], &dest).unwrap(), 5);
        assert_eq!(fs::read(&dest).unwrap(), b"whole");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_copy_leaves_neither_a_partial_file_nor_a_temp_file() {
        struct Dies(u8);
        impl std::io::Read for Dies {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
                }
                self.0 -= 1;
                buf[..4].copy_from_slice(b"data");
                Ok(4)
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("1_A.mp4");
        assert!(copy_reader_durably(&mut Dies(3), &dest).is_err());
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0, "a partial file was left behind");
        // And a good copy appears whole, with no temp file next to it.
        assert_eq!(copy_reader_durably(&mut &b"whole"[..], &dest).unwrap(), 5);
        assert_eq!(fs::read(&dest).unwrap(), b"whole");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn addresses_in_a_sidecar_are_cut_to_host_and_path_and_secret_keys_dropped() {
        let v = scrub(json!({
            "url": "https://cdn.example.gr/v/clip.mp4?token=SECRET123&sig=abc",
            "cookie_header": "sid=SECRET",
            "list": [{"u": "//cdn.example.gr/a.m3u8?x=1", "score": 3}],
        }));
        let text = v.to_string();
        assert!(!text.contains("SECRET") && !text.contains('?') && !text.contains("https://"), "{text}");
        assert!(text.contains("cdn.example.gr/v/clip.mp4"));
        assert!(v.get("cookie_header").is_none());
    }
}
