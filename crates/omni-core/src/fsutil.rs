//! Small filesystem helpers that std does not offer.

use std::io;
use std::path::Path;

/// Rename `src` to `dst`, failing with `ErrorKind::AlreadyExists` when `dst`
/// exists. std's `rename` replaces an existing target on Windows, which must
/// never happen in the watchfolder: a file Dalet already ingested is not ours to
/// replace (defect D-05). Written through to disk (`MOVEFILE_WRITE_THROUGH`).
#[cfg(windows)]
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(std::iter::once(0)).collect() };
    let (s, d) = (wide(src), wide(dst));
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    unsafe { MoveFileExW(PCWSTR(s.as_ptr()), PCWSTR(d.as_ptr()), MOVEFILE_WRITE_THROUGH) }.map_err(|e| {
        // An HRESULT built from a Win32 error carries the code in its low word.
        io::Error::from_raw_os_error((e.code().0 as u32 & 0xFFFF) as i32)
    })
}

/// Non-Windows: a hard link fails when `dst` exists, then the source goes.
#[cfg(not(windows))]
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::hard_link(src, dst)?;
    std::fs::remove_file(src)
}

/// Whether a reparse tag names another place (the name-surrogate bit,
/// `IsReparseTagNameSurrogate`): a symbolic link (0xA000000C), a junction or
/// volume mount point (0xA0000003). Not a deduplicated file (0x80000013), a
/// WOF-compressed one (0x80000017) or a cloud placeholder, which are ordinary
/// files that only store their data differently.
pub fn is_name_surrogate(reparse_tag: u32) -> bool {
    reparse_tag & 0x2000_0000 != 0
}

/// Whether `path` is a link: a symbolic link, junction or volume mount point,
/// judged on the entry itself, never on where it points ([`is_name_surrogate`]).
/// A missing path and an ordinary file that happens to be a reparse point
/// (Data Deduplication, `compact /exe`, cloud files) are not.
pub fn is_link(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return false };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return false;
        }
        // The tag cannot be read: treated as a link, the safe reading for an elevated writer.
        reparse_tag(path).map_or(true, is_name_surrogate)
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// The reparse tag of the entry `path` itself (0 for one that is no reparse
/// point), read from a handle opened on the entry, never on its target
/// (`FILE_FLAG_OPEN_REPARSE_POINT`). The name is given in its `\\?\` form, so
/// a path of 260 characters or more is read like any other.
#[cfg(windows)]
fn reparse_tag(path: &Path) -> Option<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_TAG_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let name: Vec<u16> = verbatim(path)?.into_iter().chain(std::iter::once(0)).collect();
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the name is NUL-terminated and outlives the call; `info` is the
    // buffer of the size given; the handle is closed once.
    unsafe {
        let handle = CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .ok()?;
        let read = GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            &mut info as *mut FILE_ATTRIBUTE_TAG_INFO as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        );
        let _ = CloseHandle(handle);
        read.ok()?;
    }
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    Some(if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 { 0 } else { info.ReparseTag })
}

/// `path` made absolute and in its `\\?\` (or `\\?\UNC\`) form, which Windows
/// takes at any length. `std::path::absolute` resolves `.`, `..` and `/` first,
/// which the verbatim form would not.
#[cfg(windows)]
fn verbatim(path: &Path) -> Option<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let abs: Vec<u16> = std::path::absolute(path).ok()?.as_os_str().encode_wide().collect();
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
    if abs.starts_with(&wide(r"\\?\")) || abs.starts_with(&wide(r"\\.\")) {
        return Some(abs);
    }
    if abs.starts_with(&wide(r"\\")) {
        return Some(wide(r"\\?\UNC\").into_iter().chain(abs[2..].iter().copied()).collect());
    }
    Some(wide(r"\\?\").into_iter().chain(abs).collect())
}

/// The first link (by `is_link`) on the way from below `root` down to `path`
/// itself, `root` excluded; `path` itself when it is not under `root` at all.
/// `None` means an elevated process may write or delete there without being
/// sent somewhere else by a folder the service account could have replaced.
pub fn link_below(root: &Path, path: &Path, is_link: &dyn Fn(&Path) -> bool) -> Option<std::path::PathBuf> {
    if !path.starts_with(root) || path == root {
        return (path != root).then(|| path.to_path_buf());
    }
    let mut chain: Vec<&Path> = path.ancestors().take_while(|a| *a != root).collect();
    chain.reverse();
    chain.into_iter().find(|p| is_link(p)).map(Path::to_path_buf)
}

/// The first link (by `is_link`) at `path` or anywhere below it, never
/// descending into a link it finds. What `icacls /T` or a recursive grant
/// would follow out of the tree. A missing path has none.
pub fn first_link_in_tree(path: &Path, is_link: &dyn Fn(&Path) -> bool) -> Option<std::path::PathBuf> {
    first_link_in_tree_with(path, is_link, &list_dir)
}

/// The sorted entries of a real folder (not a link); `None` for anything else.
fn list_dir(path: &Path) -> Option<Vec<std::path::PathBuf>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() {
        return None;
    }
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(path).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    Some(entries)
}

/// [`first_link_in_tree`] with the folder lister injected. Depth-first in the
/// lister's order, with an explicit stack: a deliberately deep chain of folders
/// (a watchfolder anyone may write) cannot overflow the call stack.
pub fn first_link_in_tree_with(
    path: &Path,
    is_link: &dyn Fn(&Path) -> bool,
    list: &dyn Fn(&Path) -> Option<Vec<std::path::PathBuf>>,
) -> Option<std::path::PathBuf> {
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        if is_link(&p) {
            return Some(p);
        }
        if let Some(entries) = list(&p) {
            // Reversed, so the first entry is looked at first.
            stack.extend(entries.into_iter().rev());
        }
    }
    None
}

/// Refuse to write into `dir` (one the service account can write: `bin`,
/// `data`…) when it is a link: the files would land wherever it points.
pub fn ensure_not_link(dir: &Path, is_link: &dyn Fn(&Path) -> bool) -> io::Result<()> {
    if is_link(dir) {
        return Err(io::Error::other(format!(
            "{} is a link (junction or symbolic link), not a folder; Setup does not write through it. Remove the link and run Setup again.",
            dir.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_anywhere_below_the_root_is_found_and_the_root_itself_is_not_judged() {
        let root = Path::new(r"C:\Omni");
        let item = root.join("data").join("adblock").join("list.txt");
        let links = |names: &'static [&'static str]| move |p: &Path| names.iter().any(|n| p.ends_with(n));
        assert_eq!(link_below(root, &item, &links(&[])), None);
        assert_eq!(link_below(root, &item, &links(&["data"])), Some(root.join("data")));
        assert_eq!(link_below(root, &item, &links(&["adblock"])), Some(root.join("data").join("adblock")));
        assert_eq!(link_below(root, &item, &links(&["list.txt"])), Some(item.clone()));
        assert_eq!(link_below(root, &item, &links(&["Omni"])), None, "the root is checked elsewhere (its owner)");
        // Not under the root: refused as if it were a link.
        assert_eq!(link_below(root, Path::new(r"D:\x"), &links(&[])), Some(std::path::PathBuf::from(r"D:\x")));
        assert!(ensure_not_link(&root.join("bin"), &links(&["bin"])).is_err());
        assert!(ensure_not_link(&root.join("bin"), &links(&[])).is_ok());
    }

    #[test]
    fn a_link_deep_in_a_tree_is_found_and_never_entered() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("data");
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        std::fs::create_dir_all(root.join("link").join("inside")).unwrap();
        std::fs::write(root.join("a").join("b").join("f.txt"), b"x").unwrap();
        let none = |_: &Path| false;
        assert_eq!(first_link_in_tree(&root, &none), None);
        let b = root.join("a").join("b");
        assert_eq!(first_link_in_tree(&root, &|p: &Path| p == b), Some(b.clone()));
        // Inside a link nothing is looked at.
        let link = root.join("link");
        let seen = std::cell::RefCell::new(Vec::new());
        let probe = |p: &Path| {
            seen.borrow_mut().push(p.to_path_buf());
            p == link
        };
        assert_eq!(first_link_in_tree(&root, &probe), Some(link.clone()));
        assert!(!seen.borrow().iter().any(|p| p.starts_with(&link) && *p != link), "{:?}", seen.borrow());
        assert_eq!(first_link_in_tree(&root, &|p: &Path| p == root), Some(root.clone()));
        assert_eq!(first_link_in_tree(&d.path().join("missing"), &none), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_real_directory_symlink_is_a_link() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("plain.txt"), b"x").unwrap();
        let link = d.path().join("link");
        if let Err(e) = std::os::windows::fs::symlink_dir(&target, &link) {
            eprintln!("SKIPPED a_real_directory_symlink_is_a_link: this account may not create symbolic links ({e})");
            return;
        }
        assert!(is_link(&link));
        assert!(!is_link(&target));
        assert!(!is_link(&target.join("plain.txt")));
        assert!(!is_link(&d.path().join("missing")));
        // The same answer as std, which reads the tag the same way.
        for p in [&link, &target] {
            assert_eq!(is_link(p), std::fs::symlink_metadata(p).unwrap().file_type().is_symlink(), "{p:?}");
        }
        assert_eq!(link_below(d.path(), &link.join("x"), &is_link), Some(link.clone()));
        assert_eq!(first_link_in_tree(d.path(), &is_link), Some(link.clone()));
        assert_eq!(first_link_in_tree(&target, &is_link), None);
    }

    /// P13.1b2 A5: the tag was read with `FindFirstFileW` on the plain name, which
    /// fails at 260 characters, and an unreadable tag counts as a link.
    #[cfg(windows)]
    #[test]
    fn the_tag_of_an_entry_at_a_long_path_is_read() {
        let d = tempfile::tempdir().unwrap();
        let mut dir = d.path().to_path_buf();
        for i in 0..4 {
            dir.push(format!("{}{i}", "a".repeat(80)));
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("SKIPPED the_tag_of_an_entry_at_a_long_path_is_read: Windows refused a long path here ({e})");
            return;
        }
        let file = dir.join("plain.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(file.as_os_str().len() > 260, "{}", file.as_os_str().len());
        assert_eq!(reparse_tag(&file), Some(0), "the tag of an ordinary file at a long path was not read");
        assert_eq!(reparse_tag(&dir), Some(0));
        assert!(!is_link(&file) && !is_link(&dir));
        assert_eq!(reparse_tag(&dir.join("missing")), None);
        // A real link at a long path is still one.
        let link = dir.join("link");
        match std::os::windows::fs::symlink_dir(d.path(), &link) {
            Ok(()) => {
                assert!(is_link(&link));
                assert!(reparse_tag(&link).is_some_and(is_name_surrogate));
                std::fs::remove_dir(&link).unwrap();
            }
            Err(e) => eprintln!("SKIPPED the long-path symbolic link half: this account may not create symbolic links ({e})"),
        }
    }

    /// U2: only a name surrogate sends a path elsewhere; a deduplicated or
    /// compressed file is an ordinary file that happens to be a reparse point.
    #[test]
    fn only_name_surrogate_tags_are_links() {
        assert!(is_name_surrogate(0xA000_000C), "symbolic link");
        assert!(is_name_surrogate(0xA000_0003), "junction / volume mount point");
        assert!(!is_name_surrogate(0x8000_0013), "Data Deduplication");
        assert!(!is_name_surrogate(0x8000_0017), "WOF (compact /exe)");
        assert!(!is_name_surrogate(0x9000_601A), "cloud files placeholder");
        assert!(!is_name_surrogate(0x8000_001B), "AppExecLink is not a folder link either");
    }

    /// U5: an explicit stack, so a chain far deeper than any call stack holds is walked.
    #[test]
    fn a_very_deep_chain_is_walked_without_recursion() {
        const DEPTH: usize = 200_000;
        let node = |k: usize| std::path::PathBuf::from(format!("n{k}"));
        let list = |p: &Path| {
            let k: usize = p.to_str()?.strip_prefix('n')?.parse().ok()?;
            (k < DEPTH).then(|| vec![node(k + 1)])
        };
        let last = node(DEPTH);
        assert_eq!(first_link_in_tree_with(&node(0), &|p: &Path| p == last, &list), Some(last.clone()));
        assert_eq!(first_link_in_tree_with(&node(0), &|_: &Path| false, &list), None);
        // Depth-first in the lister's order.
        let wide = |p: &Path| match p.to_str()? {
            "r" => Some(vec!["r/a".into(), "r/b".into()]),
            "r/a" => Some(vec!["r/a/x".into()]),
            _ => None,
        };
        let order = std::cell::RefCell::new(Vec::new());
        first_link_in_tree_with(Path::new("r"), &|p: &Path| { order.borrow_mut().push(p.to_string_lossy().into_owned()); false }, &wide);
        assert_eq!(*order.borrow(), ["r", "r/a", "r/a/x", "r/b"]);
    }

    #[test]
    fn an_existing_target_is_refused_untouched_and_a_free_one_is_taken() {
        let d = tempfile::tempdir().unwrap();
        let (src, taken, free) = (d.path().join("src"), d.path().join("taken"), d.path().join("free"));
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&taken, b"original").unwrap();

        let err = rename_no_replace(&src, &taken).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err:?}");
        assert_eq!(std::fs::read(&taken).unwrap(), b"original");
        assert!(src.exists());

        rename_no_replace(&src, &free).unwrap();
        assert_eq!(std::fs::read(&free).unwrap(), b"new");
        assert!(!src.exists());
    }
}
