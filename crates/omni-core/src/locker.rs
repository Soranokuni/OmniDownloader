//! File-locker links (P11.15a): which share links name one file that can be
//! fetched directly, with no account and no browser, and the address to fetch.
//!
//! Deterministic vendors only, from the link alone (pure; nothing is fetched
//! here). The file then goes through the normal pipeline: downloaded,
//! transcoded and verified like any other job.
//!
//! * Dropbox: a shared file (`/s/…`, `/scl/fi/…`) with `dl=1`.
//! * Google Drive: a shared file (`/file/d/{id}`, `open?id=`, `uc?id=`) as
//!   `drive.usercontent.google.com/download?id={id}&export=download&confirm=t`,
//!   which skips the «cannot scan this file for viruses» page of a large file.
//! * OneDrive (personal, `1drv.ms` and `onedrive.live.com`): the anonymous
//!   shares API, `api.onedrive.com/v1.0/shares/u!{base64url(link)}/root/content`.
//! * SharePoint / OneDrive for Business: an «anyone with the link» share
//!   with `download=1`.
//!
//! A folder of any vendor (many files, or a zip of them) and every other
//! locker (WeTransfer, MyAirBridge, TransferNow, Filemail, MEGA) stays a
//! manual download for MCR.

/// The address a job runs under once the browser captured its locker's file
/// (P11.15b): an `attachment://` address, so the pipeline takes the file on
/// disk (`source_path`) as its source and nothing else downloads anything.
/// Only in memory for that run; the job keeps the link that was sent.
pub const CAPTURE_URL: &str = "attachment://locker-capture";

/// The longest a browser capture may take, page and download together.
pub const CAPTURE_BUDGET: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// The bounds of one browser capture: its time, and the largest file it keeps
/// (`locker.max_file_gb`, as for a direct share).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureLimits {
    pub budget: std::time::Duration,
    pub max_bytes: u64,
}

/// The future a [`LockerCapture`] returns: the file's path, or why there is none.
pub type CaptureFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::path::PathBuf, String>> + Send + 'a>>;

/// Opens a locker page and keeps the one file it hands out (P11.15b), into
/// `dir` and nowhere else. The daemon's is the headless browser
/// (`omni_browser::capture`); tests give a fake.
pub trait LockerCapture: Send + Sync {
    fn capture<'a>(&'a self, url: &'a str, dir: &'a std::path::Path, limits: CaptureLimits) -> CaptureFuture<'a>;
}

/// What the worker does with locker links (P11.15): the two switches of
/// `locker` in config.json, the second as the capturer itself.
#[derive(Clone)]
pub struct LockerSettings {
    /// `locker.direct_shares`.
    pub direct_shares: bool,
    /// Set when `locker.browser_capture` is on.
    pub capture: Option<std::sync::Arc<dyn LockerCapture>>,
}

impl Default for LockerSettings {
    fn default() -> Self {
        Self { direct_shares: true, capture: None }
    }
}

impl std::fmt::Debug for LockerSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockerSettings").field("direct_shares", &self.direct_shares).field("capture", &self.capture.is_some()).finish()
    }
}

/// [`direct_download`], only while `direct_shares` is on: with it off, every
/// locker link waits for MCR as before P11.15a.
pub fn direct_download_if(direct_shares: bool, url: &str) -> Option<String> {
    direct_shares.then(|| direct_download(url)).flatten()
}

/// Whether a locker link is queued for a worker (PENDING) rather than left for
/// MCR (MANUAL_DOWNLOAD) when it arrives (P11.15b): a share of one file while
/// `locker.direct_shares` is on, a locker page while `locker.browser_capture`
/// is on. A folder always waits for MCR.
pub fn queued_for_a_worker(url: &str, cfg: &crate::config::LockerConfig) -> bool {
    match classify(url) {
        Locker::Direct { .. } => cfg.direct_shares,
        Locker::Manual { .. } => cfg.browser_capture,
        Locker::Folder { .. } | Locker::No => false,
    }
}

/// What a link is, as a file locker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locker {
    /// Not a file-locker link.
    No,
    /// One file, fetched directly from `url` (`vendor` names the service).
    Direct { vendor: &'static str, url: String },
    /// A shared folder: not one video. MCR picks the file.
    Folder { vendor: &'static str },
    /// A locker whose file needs a person (or, later, the browser).
    Manual { vendor: &'static str },
}

/// Lockers that need a person: (registrable domain, name).
const MANUAL: &[(&str, &str)] = &[
    ("wetransfer.com", "WeTransfer"),
    ("we.tl", "WeTransfer"),
    ("myairbridge.com", "MyAirBridge"),
    ("mab.to", "MyAirBridge"),
    ("transfernow.net", "TransferNow"),
    ("swisstransfer.com", "SwissTransfer"),
    ("filemail.com", "Filemail"),
    ("fex.net", "FEX"),
    ("mega.nz", "MEGA"),
    ("mega.io", "MEGA"),
];

/// [`Locker`] of `url`, from its parsed host and path.
pub fn classify(url: &str) -> Locker {
    let Ok(parsed) = reqwest::Url::parse(url.trim()) else { return Locker::No };
    if !matches!(parsed.scheme(), "http" | "https") {
        return Locker::No;
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let site = crate::psl::registrable(&host);
    let segs: Vec<&str> = parsed.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    let param = |name: &str| parsed.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());

    match site.as_str() {
        "dropbox.com" => dropbox(&parsed, &segs),
        "google.com" if host == "drive.google.com" => google_drive(&segs, param("id"), param("resourcekey")),
        // The short link's first segment is the item's kind: `f` a folder (P11.15a2), else a file.
        "1drv.ms" if segs.first() == Some(&"f") => Locker::Folder { vendor: "OneDrive" },
        "1drv.ms" => Locker::Direct { vendor: "OneDrive", url: onedrive_share_content(url.trim()) },
        "live.com" if host == "onedrive.live.com" => {
            // A personal share opened in the browser: `?cid=…&resid=…` or `?redeem=…`.
            if param("resid").is_some() || param("redeem").is_some() || param("id").is_some() {
                Locker::Direct { vendor: "OneDrive", url: onedrive_share_content(url.trim()) }
            } else {
                Locker::Manual { vendor: "OneDrive" }
            }
        }
        "sharepoint.com" => sharepoint(&parsed, &segs),
        // Infomaniak (P15.11) runs SwissTransfer; of its sites only a kDrive
        // share holds files.
        "infomaniak.com" if host == "kdrive.infomaniak.com" => Locker::Manual { vendor: "kDrive" },
        "infomaniak.com" => Locker::No,
        _ => match MANUAL.iter().find(|(d, _)| *d == site) {
            Some((_, vendor)) => Locker::Manual { vendor },
            None => Locker::No,
        },
    }
}

/// Whether `url` is any file-locker link (direct, folder or manual).
pub fn is_locker(url: &str) -> bool {
    classify(url) != Locker::No
}

/// Whether `url` is on a file-transfer service's own sites (P15.1): the
/// lockers and their mail and asset hosts (`about.wetransfer.com`,
/// `backgrounds.wetransfer.net`, `cdn.transfernow.net`, …). MEGA is not one:
/// its links stay as [`classify`] has them.
pub fn is_transfer_service(url: &str) -> bool {
    let Some((_, host)) = web(url) else { return false };
    match crate::psl::registrable(&host).as_str() {
        "wetransfer.com" | "wetransfer.net" | "we.tl" | "transfernow.net" | "swisstransfer.com" | "myairbridge.com" | "mab.to"
        | "filemail.com" | "dropbox.com" | "sharepoint.com" | "1drv.ms" | "fex.net" => true,
        // SwissTransfer's operator: its ads, footer and account pages ride
        // along in a forwarded SwissTransfer mail (P15.11).
        "infomaniak.com" => true,
        "google.com" => host == "drive.google.com",
        "live.com" => host == "onedrive.live.com",
        _ => false,
    }
}

/// Whether `url` is the link a transfer service sends for the files
/// themselves (P15.1), as opposed to its front page, help, legal, contact,
/// account or mail-preference pages:
///
/// * WeTransfer `wetransfer.com/downloads/…` (any subdomain), `we.tl/{x}`;
/// * TransferNow `/dl/{id}…`, also after a two-letter language segment;
/// * SwissTransfer `/d/{id}` or `/dl/{id}`;
/// * MyAirBridge `mab.to/t/{id}…`, `myairbridge.com/transfer/{id}/…` and the
///   older `#!/folder/…` / `#!/link/…` pages;
/// * Filemail `/d/{id}`;
/// * FEX `fex.net/s/{id}`, also after a language segment (`/uk/s/{id}`);
/// * Infomaniak's kDrive `kdrive.infomaniak.com/app/share/…`;
/// * Dropbox (any subdomain, `dl.dropbox.com` too): a shared file or folder
///   as [`classify`] reads it, or a Dropbox Transfer `/t/{id}`;
/// * Google Drive: a shared file or folder (`/file/d/{id}`, `open?id=`,
///   `uc?id=`, `/drive/folders/…`);
/// * OneDrive: any `1drv.ms` short link, a `onedrive.live.com` share;
/// * SharePoint: an «anyone» link (`/:v:/…`, `/:f:/…`), a page that names the
///   item (`id=`, `share=`, `docid=`), or a media file's own address.
///
/// Pure: from the link alone.
pub fn is_share_link(url: &str) -> bool {
    let Some((parsed, host)) = web(url) else { return false };
    let site = crate::psl::registrable(&host);
    let segs: Vec<&str> = parsed.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    let lower: Vec<String> = segs.iter().map(|s| s.to_ascii_lowercase()).collect();
    let seg = |i: usize| lower.get(i).map(String::as_str);
    // `/{kind}/{id}`, also after a two-letter language segment (`/en/dl/{id}`).
    let kind_then_id = |kinds: &[&str]| {
        let language = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic()) && !kinds.contains(&s);
        let start = usize::from(seg(0).is_some_and(language));
        seg(start).is_some_and(|k| kinds.contains(&k)) && seg(start + 1).is_some()
    };
    let param = |name: &str| parsed.query_pairs().any(|(k, v)| k.eq_ignore_ascii_case(name) && !v.is_empty());
    let shared = || matches!(classify(url), Locker::Direct { .. } | Locker::Folder { .. });
    match site.as_str() {
        "wetransfer.com" => seg(0) == Some("downloads") && seg(1).is_some(),
        "we.tl" => seg(0).is_some(),
        "transfernow.net" => kind_then_id(&["dl"]),
        "swisstransfer.com" => kind_then_id(&["d", "dl"]),
        "mab.to" => seg(0) == Some("t") && seg(1).is_some(),
        "myairbridge.com" => {
            let fragment = parsed.fragment().unwrap_or("").to_ascii_lowercase();
            kind_then_id(&["transfer"]) || fragment.contains("/folder/") || fragment.contains("/link/")
        }
        "filemail.com" => seg(0) == Some("d") && seg(1).is_some(),
        "fex.net" => kind_then_id(&["s"]),
        "infomaniak.com" => host == "kdrive.infomaniak.com" && seg(0) == Some("app") && seg(1) == Some("share") && seg(2).is_some(),
        // Any subdomain (`dl.dropbox.com/s/…`): the path's shape decides.
        "dropbox.com" => shared() || (seg(0) == Some("t") && seg(1).is_some()),
        "google.com" if host == "drive.google.com" => {
            shared()
                || (seg(0) == Some("file") && seg(1) == Some("d") && seg(2).is_some())
                || (matches!(seg(0), Some("open" | "uc")) && segs.len() == 1 && param("id"))
        }
        "1drv.ms" => seg(0).is_some(),
        "live.com" if host == "onedrive.live.com" => shared(),
        "sharepoint.com" => {
            const MEDIA: &[&str] = &[".mp4", ".mov", ".mxf", ".avi", ".mkv", ".m4v", ".mts", ".wav", ".mp3", ".zip"];
            shared()
                || param("id")
                || param("share")
                || param("docid")
                || lower.last().is_some_and(|l| MEDIA.iter().any(|e| l.ends_with(e)))
        }
        _ => false,
    }
}

/// One name for the links of one transfer that a service sends twice in a
/// mail (P15.1): MyAirBridge's `mab.to/t/{id}/…` and its «alternate link»
/// `myairbridge.com/transfer/{id}/…` are the same files.
pub fn share_identity(url: &str) -> Option<String> {
    let (parsed, host) = web(url)?;
    let segs: Vec<&str> = parsed.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    let id = match (crate::psl::registrable(&host).as_str(), segs.as_slice()) {
        ("mab.to", [t, id, ..]) if t.eq_ignore_ascii_case("t") => *id,
        ("myairbridge.com", [t, id, ..]) if t.eq_ignore_ascii_case("transfer") => *id,
        ("myairbridge.com", [_, t, id, ..]) if t.eq_ignore_ascii_case("transfer") => *id,
        _ => return None,
    };
    Some(format!("MyAirBridge/{id}"))
}

/// `url` parsed, when it is a web address, with its lowercased host.
fn web(url: &str) -> Option<(reqwest::Url, String)> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some((parsed, host))
}

/// The direct address of `url`'s one file, when it has one.
pub fn direct_download(url: &str) -> Option<String> {
    match classify(url) {
        Locker::Direct { url, .. } => Some(url),
        _ => None,
    }
}

/// The parts of a direct address that open the share (P11.15a2): every query
/// value and every path segment of 10 characters or more (keys, ids, the
/// shares API's `u!…` token). Given to the log redactor, so no log line shows
/// them, whichever tool prints the address.
///
/// Only the parts that open the share (P11.15a3), never a constant like
/// `download`: Google Drive's file id and resource key, Dropbox's key or id and
/// `rlkey`, the OneDrive shares API's `u!…` token, a SharePoint link's share
/// token and its `e` code. At most [`MAX_SECRET_PARTS`], each of 6 characters
/// or more, without repeats.
pub fn secret_parts(direct: &str) -> Vec<String> {
    let Ok(parsed) = reqwest::Url::parse(direct) else { return Vec::new() };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let segs: Vec<String> = parsed.path_segments().map(|s| s.map(str::to_string).collect()).unwrap_or_default();
    let param = |name: &str| parsed.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    let mut out: Vec<String> = Vec::new();
    match host.as_str() {
        "drive.usercontent.google.com" => out.extend(param("id").into_iter().chain(param("resourcekey"))),
        h if h == "dropbox.com" || h.ends_with(".dropbox.com") => {
            match segs.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
                ["s", key, ..] => out.push(key.to_string()),
                ["scl", "fi", id, ..] => out.push(id.to_string()),
                _ => {}
            }
            out.extend(param("rlkey"));
        }
        "api.onedrive.com" => out.extend(segs.iter().filter(|s| s.starts_with("u!")).cloned()),
        h if h.ends_with(".sharepoint.com") => {
            // `/:v:/g/personal/{user}/{token}`: the last segment is the share's token.
            out.extend(segs.last().cloned());
            out.extend(param("e"));
        }
        _ => {}
    }
    let mut kept: Vec<String> = Vec::new();
    for part in out.into_iter().filter(|p| p.len() >= 6) {
        if !kept.contains(&part) && kept.len() < MAX_SECRET_PARTS {
            kept.push(part);
        }
    }
    kept
}

/// The most parts [`secret_parts`] gives for one address.
pub const MAX_SECRET_PARTS: usize = 4;

/// `text` with every web address in it replaced by `sent` (P11.15a2): a
/// tool's error about a share's direct address (its key, a signed download
/// link it was sent on to) says the link that was sent instead, on the card,
/// in the job's history and in the log.
pub fn scrub_addresses(text: &str, sent: &str) -> String {
    replace_addresses(text, |_| sent.to_string())
}

/// `text` with every web address in it replaced by its site
/// ([`crate::urlnorm::site_for_log`]) (P11.15b2): what a capture's failure
/// says names the site it was refused or failed on, never its address.
pub fn addresses_to_sites(text: &str) -> String {
    replace_addresses(text, crate::urlnorm::site_for_log)
}

fn replace_addresses(text: &str, with: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("http://").into_iter().chain(rest.find("https://")).min() {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let mut end = tail.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']')).unwrap_or(tail.len());
        // A sentence's punctuation after the address is not part of it.
        while end > 0 && tail[..end].ends_with([':', ',', '.', ';']) {
            end -= 1;
        }
        out.push_str(&with(&tail[..end]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The directory under `temp` that holds every job's capture directory.
pub const CAPTURES_DIR: &str = "captures";

/// Where a locker capture of job `job_id` keeps the page's file (P11.15b2):
/// `temp/captures/{id}`, outside the job's workspace, so the pipeline's
/// clean-up of a failed run does not take it and a retry finds it. One
/// directory per job, removed as a whole, never matched by prefix.
pub fn capture_dir(temp: &std::path::Path, job_id: i64) -> std::path::PathBuf {
    temp.join(CAPTURES_DIR).join(job_id.to_string())
}

/// Remove job `job_id`'s capture directory, if any.
pub fn remove_capture_dir(temp: &std::path::Path, job_id: i64) {
    let dir = capture_dir(temp, job_id);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Job #{job_id}: the captured file could not be removed yet ({e}); the retention sweep removes it");
        }
    }
}

/// The file a finished capture left in `dir` (`files/capture` or
/// `files/capture.{ext}`, named so only after its checks), if any. A partial
/// download is never named so.
pub fn captured_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir.join("files")).ok()?;
    entries.flatten().map(|e| e.path()).find(|p| {
        p.is_file()
            && p.file_stem().and_then(|s| s.to_str()) == Some("capture")
            && std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
    })
}

fn dropbox(parsed: &reqwest::Url, segs: &[&str]) -> Locker {
    let vendor = "Dropbox";
    match segs {
        // A shared file: the old `/s/{key}/{name}` and the new `/scl/fi/{id}/{name}?rlkey=…`.
        ["s", _, _, ..] | ["scl", "fi", _, _, ..] => {
            let mut direct = parsed.clone();
            let kept: Vec<(String, String)> = parsed
                .query_pairs()
                .filter(|(k, _)| k != "dl" && k != "raw")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            direct.set_fragment(None);
            {
                let mut q = direct.query_pairs_mut();
                q.clear();
                for (k, v) in &kept {
                    q.append_pair(k, v);
                }
                q.append_pair("dl", "1");
            }
            Locker::Direct { vendor, url: direct.to_string() }
        }
        // A shared folder: `/sh/…` and `/scl/fo/…`.
        ["sh", ..] | ["scl", "fo", ..] => Locker::Folder { vendor },
        _ => Locker::Manual { vendor },
    }
}

fn google_drive(segs: &[&str], id_param: Option<String>, resource_key: Option<String>) -> Locker {
    let vendor = "Google Drive";
    let id = match segs {
        ["file", "d", id, ..] => Some(id.to_string()),
        ["open"] | ["uc"] => id_param,
        ["drive", "folders", ..] | ["drive", "u", _, "folders", ..] => return Locker::Folder { vendor },
        _ => None,
    };
    let plain = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    match id.filter(|id| id.len() >= 10 && plain(id)) {
        Some(id) => {
            let mut url = format!("https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t");
            // An older shared file needs its resource key too (P11.15a3).
            if let Some(key) = resource_key.filter(|k| !k.is_empty() && plain(k)) {
                url.push_str(&format!("&resourcekey={key}"));
            }
            Locker::Direct { vendor, url }
        }
        None => Locker::Manual { vendor },
    }
}

fn sharepoint(parsed: &reqwest::Url, segs: &[&str]) -> Locker {
    let vendor = "SharePoint";
    // An «anyone» share link: `/:v:/g/…`, `/:u:/r/…` (`:f:` is a folder).
    match segs.first() {
        Some(kind) if kind.len() == 3 && kind.starts_with(':') && kind.ends_with(':') => {
            if *kind == ":f:" {
                return Locker::Folder { vendor };
            }
            let mut direct = parsed.clone();
            direct.set_fragment(None);
            let kept: Vec<(String, String)> =
                parsed.query_pairs().filter(|(k, _)| k != "download").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
            {
                let mut q = direct.query_pairs_mut();
                q.clear();
                for (k, v) in &kept {
                    q.append_pair(k, v);
                }
                q.append_pair("download", "1");
            }
            Locker::Direct { vendor, url: direct.to_string() }
        }
        _ => Locker::Manual { vendor },
    }
}

/// The anonymous OneDrive shares API address of a personal share link: the
/// link encoded as `u!` + base64url without padding.
fn onedrive_share_content(link: &str) -> String {
    format!("https://api.onedrive.com/v1.0/shares/u!{}/root/content", base64url(link.as_bytes()))
}

/// Base64 with the URL-safe alphabet and no padding (RFC 4648 §5).
fn base64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(url: &str) -> String {
        match classify(url) {
            Locker::Direct { url, .. } => url,
            other => panic!("{url}: {other:?}"),
        }
    }

    #[test]
    fn dropbox_files_are_fetched_with_dl_1_and_folders_are_not() {
        assert_eq!(direct("https://www.dropbox.com/s/abc123/clip.mp4?dl=0"), "https://www.dropbox.com/s/abc123/clip.mp4?dl=1");
        assert_eq!(
            direct("https://www.dropbox.com/scl/fi/x1y2z3/clip.mov?rlkey=k9k9&e=1&dl=0"),
            "https://www.dropbox.com/scl/fi/x1y2z3/clip.mov?rlkey=k9k9&e=1&dl=1"
        );
        assert_eq!(direct("https://dropbox.com/s/abc123/clip.mp4"), "https://dropbox.com/s/abc123/clip.mp4?dl=1");
        assert_eq!(classify("https://www.dropbox.com/sh/abc/AAB?dl=0"), Locker::Folder { vendor: "Dropbox" });
        assert_eq!(classify("https://www.dropbox.com/scl/fo/abc/xyz?rlkey=1"), Locker::Folder { vendor: "Dropbox" });
        assert_eq!(classify("https://www.dropbox.com/home"), Locker::Manual { vendor: "Dropbox" });
    }

    #[test]
    fn google_drive_files_are_fetched_from_the_download_address() {
        let want = "https://drive.usercontent.google.com/download?id=1AbC_dEf-GhIjKlMn&export=download&confirm=t";
        assert_eq!(direct("https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?usp=sharing"), want);
        assert_eq!(direct("https://drive.google.com/open?id=1AbC_dEf-GhIjKlMn"), want);
        assert_eq!(direct("https://drive.google.com/uc?id=1AbC_dEf-GhIjKlMn&export=download"), want);
        assert_eq!(classify("https://drive.google.com/drive/folders/1AbCdEfGhIj"), Locker::Folder { vendor: "Google Drive" });
        assert_eq!(classify("https://drive.google.com/file/d/short/view"), Locker::Manual { vendor: "Google Drive" });
        assert_eq!(classify("https://www.google.com/search?q=drive"), Locker::No);
    }

    #[test]
    fn onedrive_and_sharepoint_shares() {
        // The shares API's own example: "u!" + base64url of the link, no padding.
        assert_eq!(base64url(b"https://onedrive.live.com/redir?resid=1231244193912!12&authKey=1201919!12921!1"),
            "aHR0cHM6Ly9vbmVkcml2ZS5saXZlLmNvbS9yZWRpcj9yZXNpZD0xMjMxMjQ0MTkzOTEyITEyJmF1dGhLZXk9MTIwMTkxOSExMjkyMSEx");
        assert_eq!(base64url(b"a"), "YQ");
        assert_eq!(base64url(b"ab"), "YWI");
        assert_eq!(base64url(b"\xfb\xff"), "-_8");
        assert_eq!(
            direct("https://1drv.ms/v/s!AkExample123"),
            format!("https://api.onedrive.com/v1.0/shares/u!{}/root/content", base64url(b"https://1drv.ms/v/s!AkExample123"))
        );
        assert!(direct("https://onedrive.live.com/?cid=ABC&resid=ABC%21123&authkey=x").starts_with("https://api.onedrive.com/v1.0/shares/u!"));
        // P11.15a2: a shared folder's short link is not one file.
        assert_eq!(classify("https://1drv.ms/f/s!AkFolder123"), Locker::Folder { vendor: "OneDrive" });
        assert!(matches!(classify("https://1drv.ms/u/s!AkFile123"), Locker::Direct { .. }));
        assert_eq!(
            direct("https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EaBcD?e=Xy1"),
            "https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EaBcD?e=Xy1&download=1"
        );
        assert_eq!(classify("https://station.sharepoint.com/:f:/s/news/EaBcD"), Locker::Folder { vendor: "SharePoint" });
        assert_eq!(classify("https://station.sharepoint.com/sites/news/Shared%20Documents"), Locker::Manual { vendor: "SharePoint" });
    }

    /// P11.15a2: a tool's error text never keeps a direct address, and its keys go to the redactor.
    #[test]
    fn a_direct_address_is_scrubbed_from_text_and_its_keys_are_redacted() {
        let sent = "https://www.dropbox.com/scl/fi/x1y2z3/clip.mov?rlkey=k9k9&dl=0";
        let direct = direct(sent);
        let text = format!("ERROR: [generic] Unable to download {direct}: HTTP Error 403 (from https://uc12.dl.dropboxusercontent.com/cd/0/get/SIGNED?x=1)");
        let out = scrub_addresses(&text, sent);
        assert!(!out.contains("dl=1") && !out.contains("SIGNED") && !out.contains("dropboxusercontent"), "{out}");
        assert_eq!(out.matches(sent).count(), 2, "{out}");
        assert!(out.starts_with("ERROR: [generic] Unable to download ") && out.contains(": HTTP Error 403 (from "));
        assert_eq!(scrub_addresses("no address here", sent), "no address here");

        // P11.15a3: the keys a log line must not show, and nothing else (no `download`, no `1`).
        let drive = direct_download("https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?resourcekey=0-AbCdEfGh").unwrap();
        assert_eq!(secret_parts(&drive), vec!["1AbC_dEf-GhIjKlMn".to_string(), "0-AbCdEfGh".to_string()]);
        let dropbox = direct_download("https://www.dropbox.com/scl/fi/x1y2z3w4v5/clip.mov?rlkey=abcdef123456&dl=0").unwrap();
        assert_eq!(secret_parts(&dropbox), vec!["x1y2z3w4v5".to_string(), "abcdef123456".to_string()]);
        let one = direct_download("https://1drv.ms/v/s!AkExample123").unwrap();
        let parts = secret_parts(&one);
        assert!(parts.len() == 1 && parts[0].starts_with("u!") && one.contains(parts[0].as_str()), "{parts:?}");
        let sp = direct_download("https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EaBcDeFgHiJ?e=Xy1Zz9").unwrap();
        assert_eq!(secret_parts(&sp), vec!["EaBcDeFgHiJ".to_string(), "Xy1Zz9".to_string()]);
        for parts in [secret_parts(&drive), secret_parts(&dropbox), secret_parts(&sp)] {
            assert!(parts.len() <= MAX_SECRET_PARTS && !parts.iter().any(|p| p == "download" || p == "1" || p == "t"), "{parts:?}");
        }
        assert!(secret_parts("https://news.example/story?long_value=abcdefghij").is_empty(), "not a share's address");
    }

    /// P11.15a3: an older shared Drive file keeps its resource key in the direct address.
    #[test]
    fn a_drive_files_resource_key_is_kept() {
        assert_eq!(
            direct("https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?usp=sharing&resourcekey=0-AbCdEfGh"),
            "https://drive.usercontent.google.com/download?id=1AbC_dEf-GhIjKlMn&export=download&confirm=t&resourcekey=0-AbCdEfGh"
        );
        assert!(!direct("https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view").contains("resourcekey"));
    }

    /// P11.15b: which locker links a worker takes, by the two switches.
    #[test]
    fn the_switches_decide_which_locker_links_a_worker_takes() {
        let cfg = |direct_shares, browser_capture| crate::config::LockerConfig { direct_shares, browser_capture, ..Default::default() };
        let share = "https://www.dropbox.com/s/abc123/clip.mp4?dl=0";
        let page = "https://we.tl/t-abc123";
        let folder = "https://www.dropbox.com/sh/folder1/AAB?dl=0";
        for (d, b) in [(false, false), (true, false), (false, true), (true, true)] {
            let c = cfg(d, b);
            assert_eq!(queued_for_a_worker(share, &c), d, "share, {d} {b}");
            assert_eq!(queued_for_a_worker(page, &c), b, "page, {d} {b}");
            assert!(!queued_for_a_worker(folder, &c), "a folder always waits");
            assert!(!queued_for_a_worker("https://news.example/story", &c), "not a locker");
        }
        assert_eq!(direct_download_if(false, share), None);
        assert!(direct_download_if(true, share).is_some());
    }

    #[test]
    fn an_address_in_a_reason_becomes_its_own_site() {
        assert_eq!(
            addresses_to_sites("refused (https://cdn.files.example/x?token=SECRET9), then http://10.0.0.5:8080/a."),
            "refused (files.example), then 10.0.0.5."
        );
        assert_eq!(addresses_to_sites("no download started"), "no download started");
        assert_eq!(scrub_addresses("at https://a.example/k?x=1.", "we.tl"), "at we.tl.", "the share scrub is unchanged");
    }

    #[test]
    fn a_capture_folder_is_per_job_and_only_a_finished_file_counts() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(capture_dir(tmp.path(), 7), tmp.path().join("captures").join("7"));
        assert!(!capture_dir(tmp.path(), 19).starts_with(capture_dir(tmp.path(), 1)));
        let dir = capture_dir(tmp.path(), 7);
        assert_eq!(captured_file(&dir), None, "no folder");
        let files = dir.join("files");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("8f2c-guid"), b"partial").unwrap();
        std::fs::write(files.join("capture.mp4"), b"").unwrap();
        assert_eq!(captured_file(&dir), None, "a partial download or an empty file is not a capture");
        std::fs::write(files.join("capture.mp4"), b"clip").unwrap();
        assert_eq!(captured_file(&dir), Some(files.join("capture.mp4")));
        remove_capture_dir(tmp.path(), 7);
        assert!(!dir.exists());
        remove_capture_dir(tmp.path(), 7);
    }

    #[test]
    fn the_other_lockers_stay_manual_and_lookalikes_are_not_lockers() {
        for (url, vendor) in [
            ("https://we.tl/t-abc123", "WeTransfer"),
            ("https://wetransfer.com/downloads/123/456", "WeTransfer"),
            ("https://www.myairbridge.com/en/#!/folder/xyz", "MyAirBridge"),
            ("https://www.transfernow.net/dl/2026abc", "TransferNow"),
            ("https://www.filemail.com/d/abcdefg", "Filemail"),
            ("https://mega.nz/file/abc#key", "MEGA"),
        ] {
            assert_eq!(classify(url), Locker::Manual { vendor }, "{url}");
        }
        for url in [
            "https://wetransfer.com.evil.example/x",
            "https://notdropbox.com/s/a/b.mp4",
            "https://www.youtube.com/watch?v=abc",
            "attachment://clip.mp4",
            "ftp://www.dropbox.com/s/a/b.mp4",
        ] {
            assert_eq!(classify(url), Locker::No, "{url}");
        }
        assert!(is_locker("https://we.tl/t-abc") && !is_locker("https://news.example/story"));
        assert_eq!(direct_download("https://we.tl/t-abc"), None);
    }

    /// P15.11: FEX is a locker a person fetches; its share is `/s/{id}`
    /// (after a language segment too), its other pages are its own.
    #[test]
    fn fex_is_a_manual_locker() {
        for url in ["https://fex.net/s/abc1d2e", "https://fex.net/uk/s/abc1d2e", "https://fex.net/ru/s/abc1d2e?x=1"] {
            assert_eq!(classify(url), Locker::Manual { vendor: "FEX" }, "{url}");
            assert!(is_share_link(url) && is_transfer_service(url), "{url}");
        }
        for url in ["https://fex.net/", "https://fex.net/uk/", "https://fex.net/pricing", "https://fex.net/s"] {
            assert!(!is_share_link(url) && is_transfer_service(url), "{url}");
        }
    }

    /// P15.11: Infomaniak (SwissTransfer's operator) is a transfer service:
    /// its pages are never jobs; only a kDrive share holds files.
    #[test]
    fn infomaniak_pages_are_the_services_and_a_kdrive_share_is_a_locker() {
        let share = "https://kdrive.infomaniak.com/app/share/123456/ab12cd34-ef56";
        assert_eq!(classify(share), Locker::Manual { vendor: "kDrive" });
        assert!(is_share_link(share) && is_transfer_service(share));
        for url in [
            "https://www.infomaniak.com/en/hosting",
            "https://manager.infomaniak.com/v3/account",
            "https://news.infomaniak.com/promo?utm_source=swisstransfer",
            "https://kdrive.infomaniak.com/app/drive/123",
        ] {
            assert!(is_transfer_service(url) && !is_share_link(url), "{url}");
        }
        assert_eq!(classify("https://www.infomaniak.com/en/hosting"), Locker::No);
        assert!(!is_transfer_service("https://infomaniak.com.evil.example/app/share/1/x"));
    }

    /// P15.1: SwissTransfer and MyAirBridge's short host are lockers a person fetches.
    #[test]
    fn swisstransfer_and_mab_to_are_manual_lockers() {
        assert_eq!(classify("https://www.swisstransfer.com/d/0a1b2c3d-1111-2222-3333-444455556666"), Locker::Manual { vendor: "SwissTransfer" });
        assert_eq!(classify("https://mab.to/t/AbCdEf123/eu1"), Locker::Manual { vendor: "MyAirBridge" });
    }

    /// P15.1: on a transfer service only the link to the files is a share; its
    /// own pages (front, help, legal, contact, account, mail preferences,
    /// images) are not.
    #[test]
    fn only_a_transfer_services_file_link_is_a_share() {
        for url in [
            "https://wetransfer.com/downloads/a1b2c3d4e5/f6a7b8c9d0/1a2b3c?utm_source=x",
            "https://newsdesk.wetransfer.com/downloads/a1b2c3d4e5/f6a7b8c9d0",
            "https://we.tl/t-AbCdEf1234",
            "https://www.transfernow.net/dl/20261124AbCdEf/XyZ12345",
            "https://www.transfernow.net/en/dl/20261124AbCdEf/XyZ12345",
            "https://www.transfernow.net/el/dl/20261124AbCdEf",
            "https://www.swisstransfer.com/d/0a1b2c3d-1111-2222-3333-444455556666",
            "https://www.swisstransfer.com/dl/0a1b2c3d-1111-2222-3333-444455556666",
            "https://mab.to/t/AbCdEf123/eu1",
            "https://www.myairbridge.com/transfer/AbCdEf123/eu1/",
            "https://www.myairbridge.com/en/#!/folder/AbCdEf123",
            "https://www.filemail.com/d/abcdefghijklmno",
            "https://www.dropbox.com/s/abc123/clip.mp4?dl=0",
            "https://www.dropbox.com/scl/fi/x1y2z3/clip.mov?rlkey=k9k9&dl=0",
            "https://www.dropbox.com/scl/fo/abc/xyz?rlkey=1",
            "https://www.dropbox.com/t/AbCdEf123456",
            "https://dl.dropbox.com/s/abc123/clip.mp4",
            "https://dl.dropbox.com/scl/fi/x1y2z3/clip.mov?rlkey=k9k9",
            "https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?usp=sharing",
            "https://drive.google.com/file/d/short/view",
            "https://drive.google.com/open?id=1AbC_dEf-GhIjKlMn",
            "https://drive.google.com/drive/folders/1AbCdEfGhIj",
            "https://1drv.ms/v/s!AkExample123",
            "https://onedrive.live.com/?cid=ABC&resid=ABC%21123&authkey=x",
            "https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EaBcD?e=Xy1",
            "https://station.sharepoint.com/sites/news/Shared%20Documents/Forms/AllItems.aspx?id=%2Fsites%2Fnews%2Fclip.mp4",
        ] {
            assert!(is_share_link(url), "{url} is a share");
            assert!(is_transfer_service(url), "{url} is on a transfer service");
        }
        for url in [
            "https://www.transfernow.net/en",
            "https://www.transfernow.net/en/about",
            "https://www.transfernow.net/en/support",
            "https://www.transfernow.net/en/legal/terms",
            "https://www.transfernow.net/en/legal/privacy-gdpr",
            "https://www.transfernow.net/en/contact?category=ABUSE&subCat=REPORT&link=https%3A%2F%2Fwww.transfernow.net%2Fdl%2Fx",
            "https://www.transfernow.net/api/mailer/subscriptions/recipient.html?email=a%40example.gr&token=t0k3n",
            "https://cdn.transfernow.net/mail/logo.png",
            "https://about.wetransfer.com/terms",
            "https://wetransfer.com/",
            "https://wetransfer.com/downloads",
            "https://backgrounds.wetransfer.net/x.png",
            "https://info.myairbridge.com/en/help",
            "https://www.myairbridge.com/en/",
            "https://www.dropbox.com/account/plan",
            "https://www.dropbox.com/plans",
            "https://www.dropbox.com/home",
            "https://help.dropbox.com/share/create-and-view-links",
            "https://em-s.dropbox.com/l/abc?x=1",
            "https://www.swisstransfer.com/transfers/0a1b2c3d-1111-2222-3333-444455556666/delete?delete-signature=abc",
            "https://www.swisstransfer.com/en-gb/report",
            "https://www.filemail.com/pricing",
            "https://drive.google.com/drive/my-drive",
            "https://onedrive.live.com/about/en-us/",
            "https://station.sharepoint.com/sites/news",
        ] {
            assert!(!is_share_link(url), "{url} is not a share");
            assert!(is_transfer_service(url), "{url} is on a transfer service");
        }
        // As at HEAD: a `dl.dropbox.com` share is still fetched directly, and
        // `dropboxusercontent.com` is not a locker at all.
        assert!(matches!(classify("https://dl.dropbox.com/s/abc123/clip.mp4"), Locker::Direct { vendor: "Dropbox", .. }));
        assert_eq!(classify("https://dl.dropboxusercontent.com/s/abc123/clip.mp4"), Locker::No);
        for url in ["https://dl.dropboxusercontent.com/s/abc123/clip.mp4", "https://www.youtube.com/watch?v=abc", "https://mega.nz/file/abc#key", "https://outlook.live.com/mail", "https://www.google.com/maps", "attachment://clip.mp4"] {
            assert!(!is_transfer_service(url), "{url}");
        }
    }

    #[test]
    fn a_myairbridge_transfer_has_one_identity_under_both_its_links() {
        let short = share_identity("https://mab.to/t/AbCdEf123/eu1");
        assert_eq!(short.as_deref(), Some("MyAirBridge/AbCdEf123"));
        assert_eq!(share_identity("https://www.myairbridge.com/transfer/AbCdEf123/eu1/"), short);
        assert_eq!(share_identity("https://www.myairbridge.com/en/transfer/AbCdEf123/eu1/"), short);
        assert_ne!(share_identity("https://mab.to/t/Other99/eu1"), short);
        assert_eq!(share_identity("https://we.tl/t-AbCdEf123"), None);
    }
}
