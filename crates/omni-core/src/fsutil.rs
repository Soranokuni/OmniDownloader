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

#[cfg(test)]
mod tests {
    use super::*;

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
