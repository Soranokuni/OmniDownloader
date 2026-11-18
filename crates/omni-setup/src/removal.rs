//! What `/uninstall` may delete, decided before anything is touched.
//!
//! Only what the installer put into the install folder goes (never a blanket
//! delete of the folder); with `/remove-data` also the configuration and the
//! data, logs, work and archive folders that lie inside it. The watchfolder
//! and everything in it stay, wherever it is: delivered videos belong to playout.

use std::io;
use std::path::{Component, Path, PathBuf};

use omni_cli::install::{program_files, Progress, Refusal, Step};
use omni_core::config::AppConfig;
use omni_core::paths::AppPaths;

use crate::log::SETUP_LOG;
use crate::registry::SETUP_EXE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    File(PathBuf),
    /// A folder and everything in it.
    Tree(PathBuf),
    /// A folder, only if it is empty by then.
    EmptyDir(PathBuf),
    /// Not removed; the line says why.
    Keep { path: PathBuf, why: String },
}

impl Removal {
    pub fn path(&self) -> &Path {
        match self {
            Removal::File(p) | Removal::Tree(p) | Removal::EmptyDir(p) => p,
            Removal::Keep { path, .. } => path,
        }
    }
}

/// `p` absolute, with `.` and `..` resolved and the real path when it exists
/// (so a junction is judged by where it points), case-folded.
fn norm(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        let s = c.to_string_lossy().into_owned();
        let s = match s.strip_prefix(r"\\?\UNC\") {
            Some(rest) => format!(r"\\{rest}"),
            None => s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s),
        };
        return PathBuf::from(s.to_lowercase());
    }
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    PathBuf::from(out.to_string_lossy().to_lowercase())
}

/// `inner` lies strictly inside `outer`.
fn strictly_inside(inner: &Path, outer: &Path) -> bool {
    let (i, o) = (norm(inner), norm(outer));
    i != o && i.starts_with(&o)
}

/// `a` is `b` or lies inside it.
fn within(a: &Path, b: &Path) -> bool {
    norm(a).starts_with(norm(b))
}

fn refuse(message: String) -> Refusal {
    Refusal { notice: Vec::new(), message }
}

/// Whether `root` may be uninstalled from at all. Checked before the service is touched.
pub fn check_root(root: &Path) -> Result<(), Refusal> {
    if !root.is_absolute() {
        return Err(refuse(format!("{} is not an absolute path; nothing was removed.", root.display())));
    }
    if root.parent().is_none() || norm(root).parent().is_none() {
        return Err(refuse(format!("{} is the root of a drive; Setup never uninstalls from there.", root.display())));
    }
    if root.ancestors().any(|a| a.join(".git").exists() || a.join("Cargo.toml").is_file()) {
        return Err(refuse(format!(
            "{} is a development tree (it has .git or Cargo.toml); nothing was removed. Use `omni-ingest service uninstall` there.",
            root.display()
        )));
    }
    if !root.join("omni-ingest.exe").is_file() {
        return Err(refuse(format!("{} holds no omni-ingest.exe; it is not an OmniIngest installation.", root.display())));
    }
    Ok(())
}

/// Everything `/uninstall` removes from `root`, in order, or why it removes nothing.
pub fn removal_plan(root: &Path, paths: &AppPaths, config: Option<&AppConfig>, remove_data: bool) -> Result<Vec<Removal>, Refusal> {
    check_root(root)?;
    let default = AppConfig::default();
    let config = config.unwrap_or(&default);
    let watch = paths.resolve(&config.watchfolder_path);
    let touches_watch = |p: &Path| within(p, &watch) || within(&watch, p);
    let keep_watch = |p: &Path| Removal::Keep { path: p.to_path_buf(), why: "it holds the watchfolder, which is never removed".into() };
    let mut plan = Vec::new();

    // ---- program files -------------------------------------------------------
    let mut files = program_files(&paths.root);
    files.push(paths.root.join(SETUP_EXE));
    for f in files {
        let Ok(meta) = std::fs::symlink_metadata(&f) else { continue };
        if !strictly_inside(&f, &paths.root) {
            continue;
        }
        plan.push(if touches_watch(&f) {
            keep_watch(&f)
        } else if meta.is_dir() {
            Removal::Tree(f)
        } else {
            Removal::File(f)
        });
    }
    if paths.bin.is_dir() && !touches_watch(&paths.bin) {
        plan.push(Removal::EmptyDir(paths.bin.clone()));
    }

    // ---- data ----------------------------------------------------------------
    if remove_data {
        // `config.json.prev`: the copy an upgrade keeps before rewriting it (P13.1b2), configuration too.
        let config_prev = paths.config.with_file_name(format!(
            "{}.prev",
            paths.config.file_name().unwrap_or_default().to_string_lossy()
        ));
        for f in [paths.config.clone(), config_prev, paths.root.join(SETUP_LOG)] {
            if f.is_file() && strictly_inside(&f, &paths.root) && !touches_watch(&f) {
                plan.push(Removal::File(f));
            }
        }
        let dirs = [
            paths.data.clone(),
            paths.logs.clone(),
            paths.resolve(&config.temp_path),
            paths.resolve(&config.archive.path),
        ];
        for d in dirs {
            if !d.exists() || plan.iter().any(|r| r.path() == d) {
                continue;
            }
            plan.push(if !strictly_inside(&d, &paths.root) {
                Removal::Keep { path: d, why: "it is outside the install folder".into() }
            } else if touches_watch(&d) {
                keep_watch(&d)
            } else {
                Removal::Tree(d)
            });
        }
        let db = paths.resolve(&config.database_path);
        if db.exists() && !strictly_inside(&db, &paths.data) && !strictly_inside(&db, &paths.root) {
            plan.push(Removal::Keep { path: db, why: "the database is outside the install folder".into() });
        }
    }

    // ---- the folder itself ---------------------------------------------------
    plan.push(if within(&watch, &paths.root) {
        Removal::Keep { path: paths.root.clone(), why: "the watchfolder is inside it".into() }
    } else {
        Removal::EmptyDir(paths.root.clone())
    });
    Ok(plan)
}

/// Carry out a plan made for `root`. Right before each removal the item is
/// checked again: still inside `root`, and no link (by `is_link`) on the way
/// down to it or at it, so a folder the service account swapped for a junction
/// after planning sends nothing elsewhere ("kept, it is a link"). `running` is
/// this executable: it cannot delete itself, so it (and the folder, when nothing
/// else is left in it) is handed to `schedule` (delete at the next restart).
/// Returns how many removals failed.
pub fn execute_removals(
    plan: &[Removal],
    root: &Path,
    running: &Path,
    is_link: &dyn Fn(&Path) -> bool,
    schedule: &mut dyn FnMut(&Path) -> io::Result<()>,
    progress: &mut dyn Progress,
) -> usize {
    let mut failed = 0;
    let mut scheduled: Vec<PathBuf> = Vec::new();
    let say = |p: &mut dyn Progress, s: String| p.line(Step::Done, &s);
    for r in plan {
        if !matches!(r, Removal::Keep { .. }) {
            let p = r.path();
            if let Some(link) = omni_core::fsutil::link_below(root, p, is_link) {
                say(progress, format!("  Kept {}: {} is a link.", p.display(), link.display()));
                continue;
            }
            if p != root && !strictly_inside(p, root) {
                say(progress, format!("  Kept {}: it is no longer inside {}.", p.display(), root.display()));
                continue;
            }
        }
        match r {
            Removal::File(p) if norm(p) == norm(running) => match schedule(p) {
                Ok(()) => {
                    scheduled.push(norm(p));
                    say(progress, format!("  {} is removed at the next restart (it is running now).", p.display()));
                }
                Err(e) => {
                    failed += 1;
                    say(progress, format!("! Could not schedule {} for removal ({e}); delete it by hand.", p.display()));
                }
            },
            Removal::File(p) => match std::fs::remove_file(p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    failed += 1;
                    say(progress, format!("! Could not remove {} ({e}).", p.display()));
                }
            },
            Removal::Tree(p) => match std::fs::remove_dir_all(p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    failed += 1;
                    say(progress, format!("! Could not remove {} ({e}).", p.display()));
                }
            },
            Removal::EmptyDir(p) => {
                if std::fs::remove_dir(p).is_ok() || !p.exists() {
                    continue;
                }
                let left: Vec<PathBuf> = std::fs::read_dir(p).map(|d| d.flatten().map(|e| norm(&e.path())).collect()).unwrap_or_default();
                if !left.is_empty() && left.iter().all(|l| scheduled.contains(l)) {
                    match schedule(p) {
                        Ok(()) => say(progress, format!("  {} is removed at the next restart.", p.display())),
                        Err(_) => say(progress, format!("  {} is left in place.", p.display())),
                    }
                } else {
                    say(progress, format!("  {} is not empty; left in place.", p.display()));
                }
            }
            Removal::Keep { path, why } => say(progress, format!("  Kept {}: {why}.", path.display())),
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lines(Vec<String>);
    impl Progress for Lines {
        fn line(&mut self, _s: Step, t: &str) {
            self.0.push(t.to_string());
        }
    }

    /// An installed root in a temp folder: exe, tools, setup stub, config and the state folders.
    fn installed() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("OmniIngest");
        for dir in ["bin", "data", "logs", "temp", "archive", "watchfolder"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for f in ["omni-ingest.exe", "omni-ingest.exe.prev", "OmniIngest-Setup.exe", "config.json", "config.json.prev", "notes-by-admin.txt"] {
            std::fs::write(root.join(f), b"x").unwrap();
        }
        for f in [
            "bin/ffmpeg.exe",
            "bin/yt-dlp.exe",
            "data/omni.db",
            "data/journalists.seed.json",
            "data/taxonomy.json",
            "logs/x.log",
            "watchfolder/CLIP.mxf",
        ] {
            std::fs::write(root.join(f), b"x").unwrap();
        }
        (d, root)
    }

    fn config(watch: &str) -> AppConfig {
        let mut c = AppConfig::default();
        c.watchfolder_path = watch.into();
        c
    }

    fn paths(root: &Path) -> AppPaths {
        AppPaths::with_root(root, "config.json")
    }

    fn removed(plan: &[Removal]) -> Vec<PathBuf> {
        plan.iter().filter(|r| !matches!(r, Removal::Keep { .. })).map(|r| r.path().to_path_buf()).collect()
    }

    #[test]
    fn without_remove_data_only_program_files_go() {
        let (_d, root) = installed();
        let plan = removal_plan(&root, &paths(&root), Some(&config(r"D:\Dalet")), false).unwrap();
        let gone = removed(&plan);
        for f in ["omni-ingest.exe", "omni-ingest.exe.prev", "OmniIngest-Setup.exe", r"bin\ffmpeg.exe", r"bin\yt-dlp.exe"] {
            assert!(gone.contains(&root.join(f)), "{f}: {plan:?}");
        }
        for state in ["config.json", "config.json.prev", "data", "logs", "temp", "archive", "notes-by-admin.txt"] {
            assert!(!gone.iter().any(|g| within(g, &root.join(state))), "{state} would go: {plan:?}");
        }
        assert!(gone.iter().all(|g| strictly_inside(g, &root) || *g == root), "{plan:?}");
        assert_eq!(plan.last(), Some(&Removal::EmptyDir(root.clone())));
        // R4: the seeds are station data once read; a plain uninstall keeps them.
        let running = root.join("OmniIngest-Setup.exe");
        let failed = execute_removals(&plan, &root, &running, &no_links, &mut |_: &Path| Ok(()), &mut Lines(Vec::new()));
        assert_eq!(failed, 0);
        for seed in ["journalists.seed.json", "taxonomy.json"] {
            assert!(root.join("data").join(seed).exists(), "{seed} was removed");
        }
        assert!(!root.join("omni-ingest.exe").exists());
    }

    fn no_links(_: &Path) -> bool {
        false
    }

    /// R3: a folder the service account swapped for a link between planning and
    /// removing sends nothing through it.
    #[test]
    fn a_folder_swapped_for_a_link_after_planning_is_kept() {
        let (_d, root) = installed();
        let plan = removal_plan(&root, &paths(&root), Some(&config(r"D:\Dalet")), true).unwrap();
        assert!(plan.contains(&Removal::Tree(root.join("data"))));
        let data = root.join("data");
        let bin = root.join("bin");
        let swapped = |p: &Path| p == data || p == bin;
        let mut lines = Lines(Vec::new());
        execute_removals(&plan, &root, &root.join("OmniIngest-Setup.exe"), &swapped, &mut |_: &Path| Ok(()), &mut lines);
        assert!(root.join("data").join("omni.db").exists(), "removed through a link");
        assert!(root.join("bin").join("ffmpeg.exe").exists(), "removed through a link");
        assert!(lines.0.iter().any(|l| l.contains("is a link")), "{:?}", lines.0);
        assert!(!root.join("logs").exists(), "what is not a link still goes");
    }

    #[test]
    fn with_remove_data_the_state_inside_the_root_goes_and_outside_it_stays() {
        let (d, root) = installed();
        let outside = d.path().join("elsewhere-archive");
        std::fs::create_dir_all(&outside).unwrap();
        let mut c = config(r"D:\Dalet");
        c.archive.path = outside.display().to_string();
        let plan = removal_plan(&root, &paths(&root), Some(&c), true).unwrap();
        let gone = removed(&plan);
        assert!(gone.contains(&root.join("config.json")));
        assert!(gone.contains(&root.join("config.json.prev")), "{plan:?}");
        for dir in ["data", "logs", "temp"] {
            assert!(plan.contains(&Removal::Tree(root.join(dir))), "{dir}: {plan:?}");
        }
        assert!(!gone.contains(&root.join("archive")), "not the configured archive");
        assert!(!gone.contains(&outside));
        assert!(plan.iter().any(|r| matches!(r, Removal::Keep { path, .. } if *path == outside)), "{plan:?}");
        assert!(gone.iter().all(|g| strictly_inside(g, &root) || *g == root), "{plan:?}");
        // An archive configured as `..\x` is outside too, however it is written.
        c.archive.path = r"..\elsewhere-archive".into();
        let plan = removal_plan(&root, &paths(&root), Some(&c), true).unwrap();
        assert!(!removed(&plan).iter().any(|g| within(g, &outside)), "{plan:?}");
    }

    #[test]
    fn the_watchfolder_inside_the_root_is_never_planned_and_keeps_the_root() {
        let (_d, root) = installed();
        let watch = root.join("watchfolder");
        for c in [config("watchfolder"), config(&watch.display().to_string())] {
            let plan = removal_plan(&root, &paths(&root), Some(&c), true).unwrap();
            assert!(!removed(&plan).iter().any(|g| within(g, &watch) || within(&watch, g)), "{plan:?}");
            assert!(plan.iter().any(|r| matches!(r, Removal::Keep { path, why } if *path == root && why.contains("watchfolder"))));
        }
        // A watchfolder inside data\ keeps data\ whole.
        let inside_data = root.join("data").join("out");
        std::fs::create_dir_all(&inside_data).unwrap();
        let plan = removal_plan(&root, &paths(&root), Some(&config(&inside_data.display().to_string())), true).unwrap();
        assert!(!removed(&plan).contains(&root.join("data")), "{plan:?}");
        // The default config (unreadable config.json) protects the default watchfolder.
        let plan = removal_plan(&root, &paths(&root), None, true).unwrap();
        assert!(!removed(&plan).iter().any(|g| within(g, &watch)), "{plan:?}");
    }

    #[test]
    fn a_development_tree_a_drive_root_and_a_foreign_folder_are_refused() {
        let (d, root) = installed();
        std::fs::write(d.path().join("Cargo.toml"), b"[workspace]").unwrap();
        let e = removal_plan(&root, &paths(&root), None, false).unwrap_err();
        assert!(e.message.contains("development tree"), "{e}");
        std::fs::remove_file(d.path().join("Cargo.toml")).unwrap();
        std::fs::create_dir(d.path().join(".git")).unwrap();
        assert!(removal_plan(&root, &paths(&root), None, false).is_err());
        std::fs::remove_dir(d.path().join(".git")).unwrap();
        assert!(removal_plan(&root, &paths(&root), None, false).is_ok());

        for drive in [r"C:\", r"\\server\share\"] {
            let e = removal_plan(Path::new(drive), &paths(Path::new(drive)), None, true).unwrap_err();
            assert!(e.message.contains("root of a drive"), "{drive}: {e}");
        }
        let e = removal_plan(Path::new(r"relative\omni"), &paths(Path::new(r"C:\x")), None, false).unwrap_err();
        assert!(e.message.contains("not an absolute path"), "{e}");
        let foreign = d.path().join("Photos");
        std::fs::create_dir(&foreign).unwrap();
        let e = removal_plan(&foreign, &paths(&foreign), None, true).unwrap_err();
        assert!(e.message.contains("no omni-ingest.exe"), "{e}");
    }

    #[test]
    fn executing_a_plan_removes_only_what_it_lists() {
        let (d, root) = installed();
        let running = root.join("OmniIngest-Setup.exe");
        let plan = removal_plan(&root, &paths(&root), Some(&config("watchfolder")), true).unwrap();
        let mut sched = Vec::new();
        let mut lines = Lines(Vec::new());
        let failed = execute_removals(&plan, &root, &running, &no_links, &mut |p: &Path| {
            sched.push(p.to_path_buf());
            Ok(())
        }, &mut lines);
        assert_eq!(failed, 0, "{:?}", lines.0);
        assert_eq!(sched, [running.clone()], "the running stub is scheduled, never deleted");
        assert!(running.exists());
        assert!(root.join("watchfolder").join("CLIP.mxf").exists(), "a delivered video is never removed");
        assert!(root.join("notes-by-admin.txt").exists(), "what the installer did not put there stays");
        for gone in ["omni-ingest.exe", "omni-ingest.exe.prev", "config.json", "config.json.prev", "bin", "data", "logs", "temp", "archive"] {
            assert!(!root.join(gone).exists(), "{gone}");
        }
        assert!(d.path().exists());
        assert!(lines.0.iter().any(|l| l.contains("next restart")));
    }

    #[test]
    fn an_empty_root_with_only_the_running_stub_is_scheduled_after_it() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Omni");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("omni-ingest.exe"), b"x").unwrap();
        std::fs::write(root.join("OmniIngest-Setup.exe"), b"x").unwrap();
        let plan = removal_plan(&root, &paths(&root), Some(&config(r"D:\Dalet")), false).unwrap();
        let mut sched = Vec::new();
        execute_removals(&plan, &root, &root.join("OmniIngest-Setup.exe"), &no_links, &mut |p: &Path| {
            sched.push(p.to_path_buf());
            Ok(())
        }, &mut Lines(Vec::new()));
        assert_eq!(sched, [root.join("OmniIngest-Setup.exe"), root.clone()]);
    }
}
