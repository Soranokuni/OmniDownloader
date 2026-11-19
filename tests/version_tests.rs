//! P14.4: `omni-ingest --version` prints "omni-ingest <version> (<commit>)".
//! `scripts\package.ps1` takes the version from this line and names the
//! release after it; the commit tells which build is installed.

use std::path::Path;
use std::process::Command;

#[test]
fn version_names_the_crate_version_and_the_commit() {
    let out = Command::new(env!("CARGO_BIN_EXE_omni-ingest")).arg("--version").output().expect("run omni-ingest --version");
    assert!(out.status.success(), "{out:?}");
    let line = String::from_utf8(out.stdout).unwrap();
    let line = line.trim();

    let rest = line.strip_prefix("omni-ingest ").unwrap_or_else(|| panic!("{line}"));
    let (version, build) = rest.split_once(' ').unwrap_or_else(|| panic!("{line}"));
    assert_eq!(version, env!("CARGO_PKG_VERSION"), "{line}");
    let build = build.strip_prefix('(').and_then(|b| b.strip_suffix(')')).unwrap_or_else(|| panic!("{line}"));
    assert!(build == "unknown" || (build.len() == 7 && build.bytes().all(|b| b.is_ascii_hexdigit())), "{line}");

    // Built inside a checkout (a plain one or a worktree), the commit is known.
    let in_checkout = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().any(|d| d.join(".git").exists());
    if in_checkout && std::env::var_os("OMNI_BUILD_SHA").is_none() {
        assert_ne!(build, "unknown", "built in a git checkout but the commit was not read: {line}");
    }
}
