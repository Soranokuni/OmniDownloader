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
        "google.com" if host == "drive.google.com" => google_drive(&segs, param("id")),
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

fn google_drive(segs: &[&str], id_param: Option<String>) -> Locker {
    let vendor = "Google Drive";
    let id = match segs {
        ["file", "d", id, ..] => Some(id.to_string()),
        ["open"] | ["uc"] => id_param,
        ["drive", "folders", ..] | ["drive", "u", _, "folders", ..] => return Locker::Folder { vendor },
        _ => None,
    };
    match id.filter(|id| id.len() >= 10 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')) {
        Some(id) => Locker::Direct {
            vendor,
            url: format!("https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t"),
        },
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
        assert_eq!(
            direct("https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EaBcD?e=Xy1"),
            "https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EaBcD?e=Xy1&download=1"
        );
        assert_eq!(classify("https://station.sharepoint.com/:f:/s/news/EaBcD"), Locker::Folder { vendor: "SharePoint" });
        assert_eq!(classify("https://station.sharepoint.com/sites/news/Shared%20Documents"), Locker::Manual { vendor: "SharePoint" });
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
