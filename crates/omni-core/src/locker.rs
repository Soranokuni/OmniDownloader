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
    ("transfernow.net", "TransferNow"),
    ("filemail.com", "Filemail"),
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
        out.push_str(sent);
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
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
}
