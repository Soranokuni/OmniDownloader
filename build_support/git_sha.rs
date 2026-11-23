// The short commit id a build came from, for `omni-ingest --version` and the
// first line of Setup's `/?` (P14.4). Shared by the build scripts of the root
// package and omni-setup through `include!`.
//
// git is not needed at build time: the id is read from the checkout's `.git`
// files (a plain checkout and a `git worktree` both work; loose and packed
// refs). `OMNI_BUILD_SHA` in the environment wins (a build from an exported
// tree); with neither, the id is "unknown".

use std::path::{Path, PathBuf};

/// Set `OMNI_GIT_SHA` for the crate being built and tell cargo when to run
/// the build script again (a commit, a checkout, a branch switch).
fn emit_git_sha() {
    println!("cargo:rerun-if-env-changed=OMNI_BUILD_SHA");
    let sha = std::env::var("OMNI_BUILD_SHA")
        .ok()
        .and_then(|s| short_sha(s.trim()))
        .or_else(|| {
            let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
            read_git_sha(&dir)
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=OMNI_GIT_SHA={sha}");
}

/// The first 7 characters of a full hexadecimal commit id; `None` for anything else.
fn short_sha(s: &str) -> Option<String> {
    let ok = s.len() >= 7 && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then(|| s[..7].to_ascii_lowercase())
}

/// Watch `path` for changes when it exists (a missing path would make cargo
/// run the script on every build).
fn watch(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// The commit HEAD points at, for the checkout holding `start`.
fn read_git_sha(start: &Path) -> Option<String> {
    let dot_git = start.ancestors().map(|d| d.join(".git")).find(|p| p.exists())?;
    // A worktree's `.git` is a file: "gitdir: <path>".
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            dot_git.parent()?.join(target)
        }
    } else {
        dot_git
    };
    // Branch refs and packed-refs live in the common directory.
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) => {
            let c = PathBuf::from(c.trim());
            if c.is_absolute() {
                c
            } else {
                git_dir.join(c)
            }
        }
        Err(_) => git_dir.clone(),
    };
    watch(&git_dir.join("HEAD"));
    watch(&git_dir.join("logs").join("HEAD"));
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(name) = head.strip_prefix("ref:").map(str::trim) else {
        // Detached HEAD: the id itself.
        return short_sha(head);
    };
    for dir in [&git_dir, &common] {
        let loose = dir.join(name);
        if let Ok(id) = std::fs::read_to_string(&loose) {
            watch(&loose);
            return short_sha(id.trim());
        }
    }
    let packed_path = common.join("packed-refs");
    watch(&packed_path);
    let packed = std::fs::read_to_string(&packed_path).ok()?;
    packed.lines().find_map(|line| {
        let (id, refname) = line.split_once(' ')?;
        (refname.trim() == name).then(|| short_sha(id)).flatten()
    })
}
