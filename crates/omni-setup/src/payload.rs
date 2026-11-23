//! The payload Setup carries: `[stub executable][zip of the release folder][footer]`.
//!
//! The footer is the last 50 bytes of the file: the magic `OMNISETUP1`, the
//! zip's length (u64, little-endian) and the SHA-256 of the zip bytes. The
//! hash proves the file arrived whole; it is not a signature.
//!
//! Only a [`VerifiedPayload`] can be extracted, so unverified bytes never reach
//! the disk. The executable is opened once, with writers locked out, and hashed
//! and extracted through that one handle.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

pub const MAGIC: &[u8; 10] = b"OMNISETUP1";
pub const FOOTER_LEN: u64 = 50;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PayloadError {
    #[error("This copy of Setup carries no files; it can only uninstall or change the account.")]
    Missing,
    #[error("This Setup file is incomplete (cut short while it was copied or downloaded). Get it again.")]
    Truncated,
    #[error("This Setup file is damaged: its files do not match their checksum. Get it again.")]
    Corrupt,
    #[error("This Setup file holds an unsafe entry ({0}); nothing was installed from it.")]
    UnsafeEntry(String),
    #[error("This Setup file's archive cannot be read: {0}")]
    Archive(String),
    #[error("Setup could not read or write its files: {0}")]
    Io(String),
}

impl From<io::Error> for PayloadError {
    fn from(e: io::Error) -> Self {
        PayloadError::Io(e.to_string())
    }
}

/// Where the zip lies in the file, and what it must hash to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadSpan {
    pub zip_start: u64,
    pub zip_len: u64,
    pub sha256: [u8; 32],
}

/// The footer for a zip of `zip_len` bytes with hash `sha256`.
pub fn footer(zip_len: u64, sha256: &[u8; 32]) -> [u8; FOOTER_LEN as usize] {
    let mut f = [0u8; FOOTER_LEN as usize];
    f[..10].copy_from_slice(MAGIC);
    f[10..18].copy_from_slice(&zip_len.to_le_bytes());
    f[18..].copy_from_slice(sha256);
    f
}

/// Where a Windows executable's own bytes end (its sections and its
/// certificate table); `None` when `head` is not a PE header.
pub fn pe_end(head: &[u8]) -> Option<u64> {
    let u16_at = |o: usize| head.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |o: usize| head.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64);
    if head.get(..2) != Some(b"MZ") {
        return None;
    }
    let pe = u32_at(0x3C)? as usize;
    if head.get(pe..pe + 4) != Some(b"PE\0\0") {
        return None;
    }
    let coff = pe + 4;
    let sections = u16_at(coff + 2)?;
    let optional = coff + 20;
    let optional_len = u16_at(coff + 16)?;
    let mut end = 0u64;
    // The certificate table (data directory 4) is the one that holds a file offset.
    let dirs = match u16_at(optional)? {
        0x10b => optional + 96,
        0x20b => optional + 112,
        _ => return None,
    };
    if dirs + 5 * 8 <= optional + optional_len {
        let (offset, size) = (u32_at(dirs + 32)?, u32_at(dirs + 36)?);
        if size > 0 {
            end = end.max(offset + size);
        }
    }
    let table = optional + optional_len;
    for s in 0..sections {
        let at = table + s * 40;
        let (size, pointer) = (u32_at(at + 16)?, u32_at(at + 20)?);
        end = end.max(pointer + size);
    }
    Some(end)
}

/// The payload of a file of `len` bytes. No magic at the end → `None` (a bare
/// stub), unless the file is a PE with bytes after its end (a Setup cut short).
/// A footer whose length does not fit the file is an error, never a panic.
pub fn find_payload<R: Read + Seek>(r: &mut R) -> Result<Option<PayloadSpan>, PayloadError> {
    let len = r.seek(SeekFrom::End(0))?;
    let read_at = |r: &mut R, at: u64, n: usize| -> Result<Vec<u8>, PayloadError> {
        let mut buf = vec![0u8; n];
        r.seek(SeekFrom::Start(at))?;
        r.read_exact(&mut buf)?;
        Ok(buf)
    };
    let trailing_bytes = |r: &mut R| -> Result<bool, PayloadError> {
        let head = read_at(r, 0, len.min(64 * 1024) as usize)?;
        Ok(pe_end(&head).is_some_and(|end| len > end))
    };
    if len < FOOTER_LEN {
        return if trailing_bytes(r)? { Err(PayloadError::Truncated) } else { Ok(None) };
    }
    let tail = read_at(r, len - FOOTER_LEN, FOOTER_LEN as usize)?;
    if &tail[..10] != MAGIC {
        return if trailing_bytes(r)? { Err(PayloadError::Truncated) } else { Ok(None) };
    }
    let zip_len = u64::from_le_bytes(tail[10..18].try_into().expect("8 bytes"));
    let room = len - FOOTER_LEN;
    if zip_len == 0 || zip_len > room {
        return Err(PayloadError::Truncated);
    }
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(&tail[18..]);
    Ok(Some(PayloadSpan { zip_start: room - zip_len, zip_len, sha256 }))
}

/// A Setup executable, opened once.
pub struct SetupFile<F> {
    inner: F,
    len: u64,
    span: Option<PayloadSpan>,
}

/// Open `path` for reading with writers locked out, and find its payload.
pub fn open_setup(path: &Path) -> Result<SetupFile<File>, PayloadError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0x1); // FILE_SHARE_READ: nobody may write it while we read it.
    }
    SetupFile::new(options.open(path)?)
}

impl<F: Read + Seek> SetupFile<F> {
    pub fn new(mut inner: F) -> Result<Self, PayloadError> {
        let span = find_payload(&mut inner)?;
        let len = inner.seek(SeekFrom::End(0))?;
        Ok(SetupFile { inner, len, span })
    }

    pub fn span(&self) -> Option<PayloadSpan> {
        self.span
    }

    /// The executable without its payload.
    pub fn stub_len(&self) -> u64 {
        self.span.map_or(self.len, |s| s.zip_start)
    }

    /// Copy the stub (the bytes before the payload) to `out`.
    pub fn write_stub(&mut self, out: &mut dyn Write) -> Result<u64, PayloadError> {
        let len = self.stub_len();
        self.inner.seek(SeekFrom::Start(0))?;
        Ok(io::copy(&mut (&mut self.inner).take(len), out)?)
    }

    /// Hash the zip; only a payload whose hash matches comes back.
    pub fn verify(mut self) -> Result<VerifiedPayload<F>, PayloadError> {
        let span = self.span.ok_or(PayloadError::Missing)?;
        self.inner.seek(SeekFrom::Start(span.zip_start))?;
        let mut hasher = Sha256::new();
        let mut zip = (&mut self.inner).take(span.zip_len);
        let mut buf = vec![0u8; 256 * 1024];
        let mut read = 0u64;
        loop {
            let n = zip.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            read += n as u64;
        }
        if read != span.zip_len {
            return Err(PayloadError::Truncated);
        }
        if hasher.finalize().as_slice() != span.sha256 {
            return Err(PayloadError::Corrupt);
        }
        Ok(VerifiedPayload { file: self, span })
    }
}

/// A payload whose bytes matched their hash. The only way to extract one.
pub struct VerifiedPayload<F> {
    file: SetupFile<F>,
    span: PayloadSpan,
}

/// A window `[start, start + len)` of a file, as a file of its own.
struct Region<'a, F> {
    inner: &'a mut F,
    start: u64,
    len: u64,
    pos: u64,
}

impl<F: Read + Seek> Read for Region<'_, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.len.saturating_sub(self.pos);
        let n = (buf.len() as u64).min(left) as usize;
        if n == 0 {
            return Ok(0);
        }
        self.inner.seek(SeekFrom::Start(self.start + self.pos))?;
        let got = self.inner.read(&mut buf[..n])?;
        self.pos += got as u64;
        Ok(got)
    }
}

impl<F: Read + Seek> Seek for Region<'_, F> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match target {
            Some(p) => {
                self.pos = p;
                Ok(p)
            }
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the payload")),
        }
    }
}

/// Device names Windows opens instead of a file of that name.
const DEVICES: &[&str] = &["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

/// `name` as a path inside the destination, or `None` when it could land
/// anywhere else: absolute, a drive prefix, a `..`, a stream or a device name.
pub fn safe_entry_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for c in Path::new(name).components() {
        match c {
            Component::Normal(part) => {
                let s = part.to_str()?;
                let stem = s.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
                let numbered = (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.len() == 4
                    && stem.as_bytes()[3].is_ascii_digit();
                if s.contains(':') || DEVICES.contains(&stem.as_str()) || numbered {
                    return None;
                }
                out.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

impl<F: Read + Seek> VerifiedPayload<F> {
    pub fn span(&self) -> PayloadSpan {
        self.span
    }

    pub fn write_stub(&mut self, out: &mut dyn Write) -> Result<u64, PayloadError> {
        self.file.write_stub(out)
    }

    /// Unpack into `dest`, an existing directory. Every entry name is checked
    /// before anything is written; nothing is ever written outside `dest`, and
    /// no existing file is overwritten.
    pub fn extract(&mut self, dest: &Path) -> Result<usize, PayloadError> {
        let region = Region { inner: &mut self.file.inner, start: self.span.zip_start, len: self.span.zip_len, pos: 0 };
        let mut archive = zip::ZipArchive::new(region).map_err(|e| PayloadError::Archive(e.to_string()))?;
        let mut plan = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let entry = archive.by_index(i).map_err(|e| PayloadError::Archive(e.to_string()))?;
            // A name that does not decode is refused like any other bad entry, before anything is written.
            let name = entry.name().map_err(|e| PayloadError::Archive(e.to_string()))?.into_owned();
            let safe = entry.enclosed_name().and_then(|_| safe_entry_path(&name));
            match safe {
                Some(rel) => plan.push((i, rel, entry.is_dir())),
                None => return Err(PayloadError::UnsafeEntry(name)),
            }
        }
        let mut files = 0;
        for (i, rel, is_dir) in plan {
            let target = dest.join(&rel);
            if is_dir {
                std::fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut entry = archive.by_index(i).map_err(|e| PayloadError::Archive(e.to_string()))?;
            let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
            io::copy(&mut entry, &mut out).map_err(|e| PayloadError::Archive(e.to_string()))?;
            out.sync_all()?;
            files += 1;
        }
        Ok(files)
    }
}

/// What `--pack` made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    pub size: u64,
    pub sha256_hex: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every file under `dir`, sorted, as (zip name with `/`, path).
fn release_files(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let zip_name = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let kind = e.file_type()?;
        if kind.is_dir() {
            release_files(&e.path(), &zip_name, out)?;
        } else if kind.is_file() {
            out.push((zip_name, e.path()));
        }
    }
    Ok(())
}

/// Removes a file when dropped, unless kept.
struct Scratch(Option<PathBuf>);

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Build `out` from `stub` (a Setup whose payload, if any, is left out) and the
/// release `folder`. Refuses a folder without `omni-ingest.exe` and an `out`
/// that exists. The zip is written to a temporary file beside `out` first.
pub fn pack(stub: &Path, folder: &Path, out: &Path) -> anyhow::Result<Packed> {
    use anyhow::{bail, Context};
    if !folder.join("omni-ingest.exe").is_file() {
        bail!("{} holds no omni-ingest.exe; give the release folder.", folder.display());
    }
    if out.exists() {
        bail!("{} exists already; Setup never overwrites a file.", out.display());
    }
    let out_abs = std::path::absolute(out).context("The output path cannot be resolved")?;
    let folder_abs = std::path::absolute(folder).context("The release folder cannot be resolved")?;
    if out_abs.starts_with(&folder_abs) {
        bail!("--out must not be inside the release folder.");
    }
    let mut source = open_setup(stub).with_context(|| format!("Cannot read {}", stub.display()))?;

    let file_name = out.file_name().context("--out needs a file name")?.to_string_lossy().into_owned();
    let temp = out.with_file_name(format!("{file_name}.{}.zip.tmp", std::process::id()));
    let mut files = Vec::new();
    release_files(folder, "", &mut files).with_context(|| format!("Cannot list {}", folder.display()))?;
    let zip_file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&temp)
        .with_context(|| format!("Cannot create {}", temp.display()))?;
    let _temp_guard = Scratch(Some(temp.clone()));
    let mut writer = zip::ZipWriter::new(zip_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    for (name, path) in &files {
        writer.start_file(name.as_str(), options).with_context(|| format!("Cannot add {name}"))?;
        let mut f = File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
        io::copy(&mut f, &mut writer).with_context(|| format!("Cannot add {name}"))?;
    }
    let mut zip_file = writer.finish().context("Cannot finish the archive")?;

    let mut out_file = std::fs::OpenOptions::new().write(true).create_new(true).open(out)
        .with_context(|| format!("Cannot create {}", out.display()))?;
    let mut out_guard = Scratch(Some(out.to_path_buf()));
    source.write_stub(&mut out_file)?;
    zip_file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut zip_len = 0u64;
    loop {
        let n = zip_file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out_file.write_all(&buf[..n])?;
        zip_len += n as u64;
    }
    let sha: [u8; 32] = hasher.finalize().into();
    out_file.write_all(&footer(zip_len, &sha))?;
    out_file.sync_all()?;
    let size = out_file.metadata()?.len();
    out_guard.0 = None;
    Ok(Packed { size, sha256_hex: hex(&sha) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal PE header: one section ending at 0x400, so the stub is 0x400 bytes.
    fn fake_stub() -> Vec<u8> {
        let mut s = vec![0u8; 0x400];
        s[..2].copy_from_slice(b"MZ");
        s[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        s[0x40..0x44].copy_from_slice(b"PE\0\0");
        let coff = 0x44;
        s[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        s[coff + 16..coff + 18].copy_from_slice(&0xF0u16.to_le_bytes());
        let opt = coff + 20;
        s[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        let sec = opt + 0xF0;
        s[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes());
        s[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes());
        s
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            w.start_file(*name, o).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn packed(stub: &[u8], zip: &[u8]) -> Vec<u8> {
        let sha: [u8; 32] = Sha256::digest(zip).into();
        let mut v = stub.to_vec();
        v.extend_from_slice(zip);
        v.extend_from_slice(&footer(zip.len() as u64, &sha));
        v
    }

    fn release() -> Vec<u8> {
        zip_of(&[("omni-ingest.exe", b"daemon"), ("bin/ffmpeg.exe", b"encoder")])
    }

    fn setup(bytes: Vec<u8>) -> Result<SetupFile<Cursor<Vec<u8>>>, PayloadError> {
        SetupFile::new(Cursor::new(bytes))
    }

    fn empty(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().next().is_none()
    }

    #[test]
    fn the_footer_round_trips_and_the_payload_extracts() {
        let stub = fake_stub();
        let zip = release();
        let bytes = packed(&stub, &zip);
        let file = setup(bytes).unwrap();
        let span = file.span().unwrap();
        assert_eq!((span.zip_start, span.zip_len), (stub.len() as u64, zip.len() as u64));
        assert_eq!(span.sha256, <[u8; 32]>::from(Sha256::digest(&zip)));
        assert_eq!(file.stub_len(), stub.len() as u64);
        let mut v = file.verify().unwrap();
        let d = tempfile::tempdir().unwrap();
        assert_eq!(v.extract(d.path()).unwrap(), 2);
        assert_eq!(std::fs::read(d.path().join("omni-ingest.exe")).unwrap(), b"daemon");
        assert_eq!(std::fs::read(d.path().join("bin").join("ffmpeg.exe")).unwrap(), b"encoder");
        let mut stub_copy = Vec::new();
        v.write_stub(&mut stub_copy).unwrap();
        assert_eq!(stub_copy, stub);
    }

    #[test]
    fn a_flipped_payload_byte_is_refused_and_nothing_is_extracted() {
        let stub = fake_stub();
        let mut bytes = packed(&stub, &release());
        bytes[stub.len() + 40] ^= 0x01;
        let d = tempfile::tempdir().unwrap();
        let result = setup(bytes).unwrap().verify().map(|mut v| v.extract(d.path()));
        assert_eq!(result.err(), Some(PayloadError::Corrupt));
        assert!(empty(d.path()));
    }

    #[test]
    fn a_file_cut_inside_the_zip_or_inside_the_footer_is_refused() {
        let stub = fake_stub();
        let bytes = packed(&stub, &release());
        for cut in [stub.len() + 10, bytes.len() - 20, bytes.len() - 45, bytes.len() - 1] {
            let err = setup(bytes[..cut].to_vec()).and_then(|f| f.verify().map(|_| ())).unwrap_err();
            assert_eq!(err, PayloadError::Truncated, "cut at {cut}");
        }
    }

    #[test]
    fn a_bare_stub_has_no_payload() {
        let file = setup(fake_stub()).unwrap();
        assert_eq!(file.span(), None);
        assert_eq!(file.stub_len(), 0x400);
        assert_eq!(file.verify().err(), Some(PayloadError::Missing));
        // Something that is not an executable at all: no payload either.
        assert_eq!(setup(b"short".to_vec()).unwrap().span(), None);
        // This test binary is a real one with nothing after its end.
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(pe_end(&me[..me.len().min(64 * 1024)]), Some(me.len() as u64));
        assert_eq!(setup(me).unwrap().span(), None);
    }

    #[test]
    fn magic_with_an_impossible_length_is_an_error_not_a_panic() {
        for len in [u64::MAX, u64::MAX - 10, 0x1_0000_0000, 0x400 + 1, 0] {
            let mut bytes = fake_stub();
            bytes.extend_from_slice(&footer(len, &[0u8; 32]));
            assert_eq!(setup(bytes).err(), Some(PayloadError::Truncated), "{len}");
        }
        // Just the footer, nothing before it.
        assert_eq!(setup(footer(1, &[0u8; 32]).to_vec()).err(), Some(PayloadError::Truncated));
    }

    #[test]
    fn an_entry_that_climbs_out_or_is_absolute_is_refused_and_nothing_is_written() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        for evil in [r"..\evil.txt", "../evil.txt", "ok/../../evil.txt", r"C:\evil.txt", "/evil.txt", r"\evil.txt", "C:evil.txt", "a/x.txt:stream", "bin/NUL.txt", "COM1"] {
            let zip = zip_of(&[("omni-ingest.exe", b"daemon"), (evil, b"pwned")]);
            let err = setup(packed(&fake_stub(), &zip)).unwrap().verify().unwrap().extract(&dest).unwrap_err();
            assert!(matches!(err, PayloadError::UnsafeEntry(_)), "{evil}: {err:?}");
            assert!(empty(&dest), "{evil}: something was written");
            assert!(!d.path().join("evil.txt").exists(), "{evil}");
        }
        assert_eq!(safe_entry_path("bin/./ffmpeg.exe"), Some(PathBuf::from("bin").join("ffmpeg.exe")));
        assert_eq!(safe_entry_path("COM10.txt"), Some(PathBuf::from("COM10.txt")));
    }

    #[test]
    fn pack_builds_a_setup_that_verifies_and_refuses_what_it_must() {
        let d = tempfile::tempdir().unwrap();
        let stub_path = d.path().join("stub.exe");
        std::fs::write(&stub_path, fake_stub()).unwrap();
        let rel = d.path().join("release");
        std::fs::create_dir_all(rel.join("bin")).unwrap();
        let out = d.path().join("out").join("Setup.exe");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        // No omni-ingest.exe: refused.
        assert!(pack(&stub_path, &rel, &out).is_err());
        std::fs::write(rel.join("omni-ingest.exe"), b"daemon").unwrap();
        std::fs::write(rel.join("bin").join("ffmpeg.exe"), b"encoder").unwrap();
        // Out inside the release folder: refused.
        assert!(pack(&stub_path, &rel, &rel.join("Setup.exe")).is_err());
        let packed = pack(&stub_path, &rel, &out).unwrap();
        assert_eq!(packed.size, std::fs::metadata(&out).unwrap().len());
        assert_eq!(packed.sha256_hex.len(), 64);
        let only: Vec<_> = std::fs::read_dir(out.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(only, ["Setup.exe"], "the temporary zip is gone");
        // An existing --out is never overwritten.
        let before = std::fs::read(&out).unwrap();
        assert!(pack(&stub_path, &rel, &out).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), before);
        // Packing from a packed Setup takes only its stub.
        let again = d.path().join("out").join("Setup2.exe");
        pack(&out, &rel, &again).unwrap();
        let file = open_setup(&again).unwrap();
        assert_eq!(file.stub_len(), 0x400);
        let mut v = file.verify().unwrap();
        let dest = d.path().join("x");
        std::fs::create_dir(&dest).unwrap();
        v.extract(&dest).unwrap();
        assert_eq!(std::fs::read(dest.join("bin").join("ffmpeg.exe")).unwrap(), b"encoder");
    }
}
