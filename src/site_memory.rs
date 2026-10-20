//! Site furniture, the daemon's side (P3.16): remember the videos the browser
//! sees on each news site's articles, and say which of a page's videos the site
//! shows on its other articles too. Such a video (a promo, a show, a
//! recommendation) is never a job's video and never queued, only offered.
//!
//! Recording is for jobs and the nightly canary pass; the benchmark and the
//! «Test URL» dry run only read. A failure here is a warning and never changes
//! what a job does: with no memory, nothing is excluded.
//!
//! A sighting is stored under the address the job (or canary) asked for, which
//! does not move between runs; the site is where the browser really ended, so a
//! shortener (`t.co`) does not lump unrelated outlets together. When counting,
//! every address the article is known by is "this article".

use std::collections::HashMap;

use tracing::warn;

use omni_browser::PageSnapshot;
use omni_core::repository::Repository;
use omni_core::site_videos::{self, Furniture, PageKey};

/// A section or listing page rather than an article: a front page (`could_be_a_story` says so), a
/// one-word section (`/video/`, `/tv/`), or a category, tag, topic, author, archive or search page.
/// Such a page shows the site's featured videos, not an article's own.
fn is_listing(page_url: &str) -> bool {
    const SECTIONS: [&str; 8] = ["video", "videos", "tv", "webtv", "live", "podcasts", "photos", "galleries"];
    const LISTING_ROOTS: [&str; 11] =
        ["category", "categories", "tag", "tags", "topic", "topics", "section", "archive", "author", "search", "page"];
    let Some(rest) = page_url.split("://").nth(1) else { return false };
    let path = rest.split(['?', '#']).next().unwrap_or("").split_once('/').map(|(_, p)| p).unwrap_or("").to_ascii_lowercase();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [only] => SECTIONS.contains(only) || LISTING_ROOTS.contains(only),
        [first, ..] => LISTING_ROOTS.contains(first),
        [] => !page_url.contains('?'),
    }
}

/// Whether the memory applies to `page_url`: an article page of a news site. Not a video platform's
/// own post (that page is the video), not a direct media address or a player page (an explicit
/// submission of one must download, whatever else the site shows), and not a front page or a
/// section or listing page (which shows the site's featured videos, not an article's own).
pub fn applies_to(page_url: &str) -> bool {
    if !omni_email::parser::could_be_a_story(page_url) || is_listing(page_url) {
        return false;
    }
    if !(page_url.starts_with("http://") || page_url.starts_with("https://")) || omni_broadcast::downloader::is_video_platform(page_url) {
        return false;
    }
    let path = page_url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    const MEDIA: [&str; 9] = [".m3u8", ".mpd", ".mp4", ".m4v", ".mov", ".webm", ".ts", ".mkv", ".mp3"];
    if MEDIA.iter().any(|e| path.ends_with(e)) {
        return false;
    }
    let after_host = path.split("://").nth(1).and_then(|r| r.split_once('/')).map(|(_, p)| p.to_string()).unwrap_or_default();
    !after_host.split('/').any(|seg| matches!(seg, "embed" | "embedded" | "player"))
}

/// The page's key, with the browser's final address and canonical when it was looked at. A page the
/// browser ended on that is no article (a video platform's page, a front page, a section) is no page of
/// the site: no key.
pub fn key_of(page_url: &str, snapshot: Option<&PageSnapshot>) -> Option<PageKey> {
    let ended = snapshot.and_then(|s| s.final_url.as_deref());
    if ended.is_some_and(|e| !applies_to(e)) {
        return None;
    }
    site_videos::page_key(page_url, ended, snapshot.and_then(|s| s.canonical.as_deref()))
}

/// Record every video the browser saw on `page_url` (`streams`, before any
/// filtering), then say which of them are site-wide: video hash to the number
/// of different articles it was seen on this week, this one included.
pub fn remember(repo: &Repository, page_url: &str, snapshot: Option<&PageSnapshot>, streams: &[String]) -> HashMap<String, usize> {
    if !applies_to(page_url) || streams.is_empty() {
        return HashMap::new();
    }
    let Some(key) = key_of(page_url, snapshot) else { return HashMap::new() };
    let hashes = site_videos::hashes_of(streams);
    let now = chrono::Utc::now();
    if let Err(e) = repo.record_site_videos(&key.domain, &key.identities, &hashes, now) {
        warn!("Site videos of {} not recorded: {e:#}", key.domain);
    }
    lookup(repo, &key, &hashes, now)
}

/// Which of `urls` are site-wide, without recording anything. The page counts as one article
/// (stored or not), under every address it is known by.
pub fn recall(repo: &Repository, page_url: &str, snapshot: Option<&PageSnapshot>, urls: &[String]) -> HashMap<String, usize> {
    if !applies_to(page_url) || urls.is_empty() {
        return HashMap::new();
    }
    let Some(key) = key_of(page_url, snapshot) else { return HashMap::new() };
    lookup(repo, &key, &site_videos::hashes_of(urls), chrono::Utc::now())
}

fn lookup(repo: &Repository, key: &PageKey, hashes: &[String], now: chrono::DateTime<chrono::Utc>) -> HashMap<String, usize> {
    match repo.site_wide(&key.domain, hashes, site_videos::window_start(now), &key.identities) {
        Ok(wide) => wide,
        Err(e) => {
            warn!("Site videos of {} not read: {e:#}", key.domain);
            HashMap::new()
        }
    }
}

/// A sniff's streams after the memory has been asked (P3.16): what stays candidates, and what
/// the site shows on its other articles too.
#[derive(Debug, PartialEq, Eq)]
pub struct SniffScreen {
    pub pool: Vec<String>,
    pub furniture: Vec<Furniture>,
    /// The furniture took out a video the page placed in the article (not a held one). Whatever
    /// filtering follows, a held video is then never taken for the article's ([`leave_nothing_held`]).
    pub removed_own: bool,
}

/// What a job does with the streams the browser found, before it chooses: record them for this
/// article, then take the site-wide ones out. An article's sibling job holds a stream or post, not
/// a page of the site: nothing is recorded or taken out.
///
/// `held` says which streams the page placed outside the article (a rail, a header, a contextual
/// list); see [`SniffScreen::removed_own`].
pub fn screen_sniff(
    repo: &Repository,
    page_url: &str,
    snapshot: Option<&PageSnapshot>,
    all_streams: &[String],
    is_sibling: bool,
    held: impl Fn(&str) -> bool,
) -> SniffScreen {
    if is_sibling {
        return SniffScreen { pool: all_streams.to_vec(), furniture: Vec::new(), removed_own: false };
    }
    let wide = remember(repo, page_url, snapshot, all_streams);
    let (pool, furniture) = site_videos::split_site_wide(all_streams, &wide);
    let removed_own = furniture.iter().any(|f| !held(&f.url));
    SniffScreen { pool, furniture, removed_own }
}

/// `streams` once every filter has run (photo-only posts, queued siblings): when the furniture took
/// out an own video of the article and no own one is left, the held ones (a rail, a header, a
/// contextual list) are not the article's either, so nothing is left. Own ones left stay, and the
/// held ones behind them are only ever offered, as `choose_primary` never walks them while an own
/// one exists. A page that had only held candidates to begin with (`removed_own` false) keeps them.
/// The held ones taken out are returned too: yt-dlp's own pick must not take them either.
pub fn leave_nothing_held(removed_own: bool, streams: Vec<String>, held: impl Fn(&str) -> bool) -> (Vec<String>, Vec<String>) {
    if removed_own && streams.iter().all(|u| held(u)) {
        (Vec::new(), streams)
    } else {
        (streams, Vec::new())
    }
}

/// The job ends for lack of a video of its own: nothing is left of the page's streams once the
/// photo-only posts are dropped, and the site's furniture was taken out.
pub fn ends_on_furniture(furniture: &[Furniture], streams_left: &[String]) -> bool {
    !furniture.is_empty() && streams_left.is_empty()
}

/// `urls` that are site-wide under the site of the address asked, and under the site of every other
/// page the article is known to be on (`ended`: where yt-dlp or the browser ended). A hand-off to a
/// player domain adds a site where nothing is recorded; the asked site is still looked up.
fn wide_under_every_site(repo: &Repository, page_url: &str, ended: [Option<&str>; 2], urls: &[String]) -> HashMap<String, usize> {
    let mut keys: Vec<PageKey> = site_videos::page_key(page_url, None, None).into_iter().collect();
    for e in ended.into_iter().flatten().filter(|u| applies_to(u)) {
        keys.extend(site_videos::page_key(page_url, Some(e), None));
    }
    let mut identities: Vec<String> = Vec::new();
    for k in &keys {
        for i in &k.identities {
            if !identities.contains(i) {
                identities.push(i.clone());
            }
        }
    }
    let mut domains: Vec<&str> = Vec::new();
    for k in &keys {
        if !domains.contains(&k.domain.as_str()) {
            domains.push(&k.domain);
        }
    }
    let hashes = site_videos::hashes_of(urls);
    let now = chrono::Utc::now();
    let mut out: HashMap<String, usize> = HashMap::new();
    for d in domains {
        match repo.site_wide(d, &hashes, site_videos::window_start(now), &identities) {
            Ok(wide) => {
                for (h, n) in wide {
                    let e = out.entry(h).or_insert(0);
                    *e = (*e).max(n);
                }
            }
            Err(e) => warn!("Site videos of {d} not read: {e:#}"),
        }
    }
    out
}

/// What yt-dlp's page scan is left with once the site's furniture is out (P3.16).
#[derive(Debug, PartialEq, Eq)]
pub struct ScanGuard {
    /// The scan's videos without the site-wide ones.
    pub videos: Vec<String>,
    /// What was taken out, scan videos and the single result's media alike, for MCR's offered list.
    pub furniture: Vec<Furniture>,
    /// Nothing but furniture is left: the scan's videos were all site-wide, or the single result's
    /// media is. yt-dlp's attempt is not made.
    pub only_furniture: bool,
}

/// yt-dlp's page scan checked against the memory, read only: the scan's `videos` (a playlist
/// of the page's videos) and its `media_urls` (the one video of a single result). `webpage_url` is
/// where yt-dlp says the page is; it names the site when it is a page of one (a shortener's
/// target), and is ignored when it is a video platform's (a hand-off to an embed).
pub fn guard_scan(
    repo: &Repository,
    page_url: &str,
    webpage_url: Option<&str>,
    browser_final: Option<&str>,
    videos: Vec<String>,
    media_urls: &[String],
    refused: &[String],
) -> ScanGuard {
    let mut seen = videos.clone();
    seen.extend(media_urls.iter().cloned());
    let wide = if applies_to(page_url) && !seen.is_empty() { wide_under_every_site(repo, page_url, [webpage_url, browser_final], &seen) } else { HashMap::new() };
    if wide.is_empty() && refused.is_empty() {
        return ScanGuard { videos, furniture: Vec::new(), only_furniture: false };
    }
    let had_videos = !videos.is_empty();
    let (kept, mut furniture) = site_videos::split_site_wide(&videos, &wide);
    let (_, dropped_media) = site_videos::split_site_wide(media_urls, &wide);
    // Held videos the browser's look already refused are not yt-dlp's pick either (they are not offered as furniture).
    let is_refused = |u: &String| refused.iter().any(|r| site_videos::video_key(r) == site_videos::video_key(u));
    let kept: Vec<String> = kept.into_iter().filter(|u| !is_refused(u)).collect();
    let media_refused = media_urls.iter().any(is_refused);
    let only_furniture = if had_videos { kept.is_empty() } else { !dropped_media.is_empty() || media_refused };
    for m in dropped_media {
        if !furniture.iter().any(|f| f.url == m.url) {
            furniture.push(m);
        }
    }
    ScanGuard { videos: kept, furniture, only_furniture }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use omni_broadcast::downloader::{choose_primary, sniffed_cands, Choice, Probe};

    const PAGE: &str = "https://www.sports.example/a/story-1";
    const OTHER_PAGE: &str = "https://www.sports.example/a/story-2";
    const YT_SHOW: &str = "https://www.youtube.com/watch?v=xZf1yNC3YDQ";
    const FB_CLIP: &str = "https://www.facebook.com/example/videos/123456/";
    const PROMO: &str = "https://media.news.example/promo-hor.mp4";
    const X_POST: &str = "https://x.com/someone/status/42";

    /// A database file in a scratch directory that removes itself.
    pub(crate) struct TestDb(pub std::path::PathBuf);
    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn repo() -> (TestDb, Repository) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("omni-site-test-{}-{nanos}-{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repository::new(dir.join("omni.db")).unwrap();
        (TestDb(dir), repo)
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// What `run_job` does with a sniff's streams: screen them (record, drop the site-wide), then choose.
    async fn chosen(repo: &Repository, page: &str, streams: &[&str]) -> (Option<String>, Vec<String>) {
        let screen = screen_sniff(repo, page, None, &s(streams), false, |_| false);
        let cands = sniffed_cands(&screen.pool, |_| false);
        let primary = match choose_primary(&cands, 3, |_| async { Probe::Ok }).await {
            Choice::Primary { url, .. } => Some(url),
            _ => None,
        };
        (primary, screen.furniture.into_iter().map(|f| f.url).collect())
    }

    #[tokio::test]
    async fn a_site_wide_inline_video_ranked_first_is_excluded_and_the_articles_own_clip_is_taken() {
        // The gazzetta shape: a long show linked on every page sat inline in the article and won on page order.
        let (_d, repo) = repo();
        // Another article of the site showed it earlier this week.
        remember(&repo, OTHER_PAGE, None, &s(&[YT_SHOW]));
        let (primary, offered) = chosen(&repo, PAGE, &[YT_SHOW, FB_CLIP]).await;
        assert_eq!(primary.as_deref(), Some(FB_CLIP));
        assert_eq!(offered, [YT_SHOW]);
    }

    #[test]
    fn a_page_whose_only_candidate_is_the_promo_ends_on_furniture_and_offers_it_with_its_count() {
        // The protothema shape: the article's real video (an X post) is dropped as videoless, the promo is all that is left.
        let (_d, repo) = repo();
        remember(&repo, OTHER_PAGE, None, &s(&[PROMO]));
        let screen = screen_sniff(&repo, PAGE, None, &s(&[PROMO, X_POST]), false, |_| false);
        assert_eq!(screen.pool, [X_POST], "only the promo is taken out");
        assert_eq!(screen.furniture.len(), 1);
        assert_eq!((screen.furniture[0].url.as_str(), screen.furniture[0].other_articles), (PROMO, 1));
        // run_job then drops the X post as a photo-only post: nothing is left, and the job ends.
        assert!(ends_on_furniture(&screen.furniture, &[]));
        assert!(!ends_on_furniture(&screen.furniture, &s(&[X_POST])), "a video that is left is the job's");
        assert!(!ends_on_furniture(&[], &[]), "no furniture: the ordinary ending");
    }

    #[test]
    fn a_siblings_streams_are_neither_recorded_nor_taken_out() {
        let (_d, repo) = repo();
        remember(&repo, OTHER_PAGE, None, &s(&[PROMO]));
        let screen = screen_sniff(&repo, PAGE, None, &s(&[PROMO]), true, |_| false);
        assert_eq!((screen.pool, screen.furniture.len()), (s(&[PROMO]), 0));
        let conn = rusqlite::Connection::open(_d.0.join("omni.db")).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "only the other article's sighting");
    }

    #[tokio::test]
    async fn a_first_sighting_is_not_excluded() {
        // Control: one article only.
        let (_d, repo) = repo();
        let (primary, offered) = chosen(&repo, PAGE, &[YT_SHOW, FB_CLIP]).await;
        assert_eq!(primary.as_deref(), Some(YT_SHOW), "the page's first choice stands");
        assert!(offered.is_empty());
        // The same article opened again is still one article.
        let (primary, offered) = chosen(&repo, PAGE, &[YT_SHOW, FB_CLIP]).await;
        assert_eq!(primary.as_deref(), Some(YT_SHOW));
        assert!(offered.is_empty());
    }

    #[test]
    fn the_same_article_under_another_final_address_is_still_one_article() {
        let (_d, repo) = repo();
        // The first run stored the article under the address asked; a retry ends elsewhere (a canonical jump).
        remember(&repo, PAGE, None, &s(&[FB_CLIP]));
        let mut snap = PageSnapshot::new(PAGE);
        snap.final_url = Some("https://www.sports.example/a/story-1-full".into());
        snap.canonical = Some("https://amp.sports.example/a/story-1".into());
        let screen = screen_sniff(&repo, PAGE, Some(&snap), &s(&[FB_CLIP]), false, |_| false);
        assert!(screen.furniture.is_empty(), "its own clip is not 'seen on 2 articles'");
        assert!(recall(&repo, PAGE, Some(&snap), &s(&[FB_CLIP])).is_empty());
        let conn = rusqlite::Connection::open(_d.0.join("omni.db")).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "still stored under the address asked, not under the one the browser ended on");
    }

    #[test]
    fn a_shortener_is_filed_under_the_site_the_browser_ended_on() {
        let (_d, repo) = repo();
        let mut snap = PageSnapshot::new("https://t.co/AbCdEf");
        snap.final_url = Some("https://www.news.example/a/story-1".into());
        remember(&repo, "https://t.co/AbCdEf", Some(&snap), &s(&[PROMO]));
        let conn = rusqlite::Connection::open(_d.0.join("omni.db")).unwrap();
        let domain: String = conn.query_row("SELECT domain FROM site_videos", [], |r| r.get(0)).unwrap();
        assert_eq!(domain, "news.example");
        // An unrelated outlet reached through the same shortener is not lumped in.
        let mut other = PageSnapshot::new("https://t.co/ZzZzZz");
        other.final_url = Some("https://www.other-outlet.example/b/story".into());
        assert!(remember(&repo, "https://t.co/ZzZzZz", Some(&other), &s(&[PROMO])).is_empty());
        // A browser that ended on a video platform's page: neither recorded nor excluded.
        let mut yt = PageSnapshot::new("https://t.co/Yyyyyy");
        yt.final_url = Some("https://www.youtube.com/watch?v=abcdefghijk".into());
        assert!(remember(&repo, "https://t.co/Yyyyyy", Some(&yt), &s(&[PROMO])).is_empty());
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn recalling_records_nothing_and_counts_the_page_as_one_article() {
        let (_d, repo) = repo();
        remember(&repo, OTHER_PAGE, None, &s(&[PROMO]));
        // One stored article and this page: two articles, though this one was never stored.
        assert_eq!(recall(&repo, PAGE, None, &s(&[PROMO])).len(), 1);
        let conn = rusqlite::Connection::open(_d.0.join("omni.db")).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "nothing was recorded by the read");
        // Once stored, the same page is not counted twice.
        remember(&repo, PAGE, None, &s(&[PROMO]));
        assert_eq!(recall(&repo, PAGE, None, &s(&[PROMO])).values().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn a_front_page_or_a_section_page_is_neither_recorded_nor_counted() {
        let (_d, repo) = repo();
        let rows = |d: &TestDb| -> i64 {
            rusqlite::Connection::open(d.0.join("omni.db")).unwrap().query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap()
        };
        // A normal article records.
        remember(&repo, PAGE, None, &s(&[FB_CLIP]));
        assert_eq!(rows(&_d), 1);
        // Section and listing pages: no record.
        for u in [
            "https://www.sports.example/",
            "https://www.sports.example/video/",
            "https://www.sports.example/category/football",
            "https://www.sports.example/tag/derby",
            "https://www.sports.example/tv",
        ] {
            assert!(remember(&repo, u, None, &s(&[YT_SHOW])).is_empty(), "{u}");
            assert!(recall(&repo, u, None, &s(&[FB_CLIP])).is_empty(), "{u}: not counted either");
        }
        assert_eq!(rows(&_d), 1, "nothing recorded for a listing");
        // An article that now redirects to the front page: neither its asked nor its final address counts.
        let mut snap = PageSnapshot::new(OTHER_PAGE);
        snap.final_url = Some("https://www.sports.example/".into());
        assert!(remember(&repo, OTHER_PAGE, Some(&snap), &s(&[YT_SHOW])).is_empty());
        assert_eq!(rows(&_d), 1);
        // A short link to a section page.
        let mut to_section = PageSnapshot::new("https://t.co/AbCdEf");
        to_section.final_url = Some("https://www.sports.example/video/".into());
        assert!(remember(&repo, "https://t.co/AbCdEf", Some(&to_section), &s(&[YT_SHOW])).is_empty());
        assert_eq!(rows(&_d), 1);
    }

    #[test]
    fn held_videos_the_screen_dropped_are_not_yt_dlps_pick_either() {
        let (_d, repo) = repo();
        on_two_articles(&repo, YT_SHOW);
        let held = |u: &str| u == FB_CLIP;
        let screen = screen_sniff(&repo, PAGE, None, &s(&[YT_SHOW, X_POST, FB_CLIP]), false, held);
        // X is dropped as a photo-only post: only the held rail video is left, and it is taken out.
        let (streams, dropped) = leave_nothing_held(screen.removed_own, s(&[FB_CLIP]), held);
        assert!(streams.is_empty());
        assert_eq!(dropped, s(&[FB_CLIP]));
        // The fallback's scan [V, W]: V is furniture, W was refused: nothing is left to download.
        let g = guard_scan(&repo, PAGE, None, None, s(&[YT_SHOW, FB_CLIP]), &[], &dropped);
        assert!(g.videos.is_empty() && g.only_furniture, "{g:?}");
        // A single html5 result whose media is W.
        let g = guard_scan(&repo, PAGE, None, None, Vec::new(), &s(&[FB_CLIP]), &dropped);
        assert!(g.only_furniture);
        // Control: without the refusal W is yt-dlp's pick.
        let g = guard_scan(&repo, PAGE, None, None, s(&[YT_SHOW, FB_CLIP]), &[], &[]);
        assert_eq!((g.videos, g.only_furniture), (s(&[FB_CLIP]), false));
        assert!(!guard_scan(&repo, PAGE, None, None, Vec::new(), &s(&[FB_CLIP]), &[]).only_furniture);
    }

    #[test]
    fn a_scan_entry_in_embed_form_matches_the_browsers_watch_form() {
        // The gazzetta shape: yt-dlp lists `embed/ID?feature=oembed`, the browser recorded `watch?v=ID`.
        let embed = "https://www.youtube.com/embed/xZf1yNC3YDQ?feature=oembed";
        let (_d, repo) = repo();
        on_two_articles(&repo, YT_SHOW);
        let g = guard_scan(&repo, PAGE, None, None, s(&[embed, FB_CLIP]), &[], &[]);
        assert_eq!(g.videos, s(&[FB_CLIP]), "the site-wide show is out of the scan");
        assert_eq!(g.furniture.len(), 1);
        assert!(guard_scan(&repo, PAGE, None, None, Vec::new(), &s(&[embed]), &[]).only_furniture);
        // A refusal recorded in the browser's form refuses the embed form too.
        let (_d2, fresh) = self::repo();
        let g = guard_scan(&fresh, PAGE, None, None, s(&[embed]), &[], &s(&[YT_SHOW]));
        assert!(g.videos.is_empty() && g.only_furniture);
        assert!(guard_scan(&fresh, PAGE, None, None, Vec::new(), &s(&[embed]), &s(&[YT_SHOW])).only_furniture);
        // Control: nothing remembered, nothing refused.
        assert_eq!(guard_scan(&fresh, PAGE, None, None, s(&[embed]), &[], &[]).videos, s(&[embed]));
    }

    #[test]
    fn a_front_page_with_a_query_is_an_article() {
        assert!(!applies_to("https://www.sports.example/"));
        assert!(applies_to("https://www.sports.example/?p=12345"), "a query-addressed article");
    }

    #[test]
    fn a_direct_media_address_or_a_player_page_is_no_article() {
        assert!(applies_to("https://www.news.example/a/story-1"));
        for u in [
            "https://cdn.news.example/v/clip.mp4",
            "https://cdn.news.example/v/index.m3u8?token=x",
            "https://www.news.example/embed/123",
            "https://www.news.example/player/123",
            "https://www.youtube.com/watch?v=abcdefghijk",
        ] {
            assert!(!applies_to(u), "{u}");
        }
        let (_d, repo) = repo();
        remember(&repo, OTHER_PAGE, None, &s(&[PROMO]));
        assert!(recall(&repo, "https://www.sports.example/v/clip.mp4", None, &s(&[PROMO])).is_empty(), "an explicit media submission is never held back");
    }

    /// Two other articles of the site showed `video` this week.
    fn on_two_articles(repo: &Repository, video: &str) {
        remember(repo, OTHER_PAGE, None, &s(&[video]));
        remember(repo, "https://www.sports.example/a/story-3", None, &s(&[video]));
    }

    #[test]
    fn a_single_html5_pick_that_is_site_wide_is_not_downloaded() {
        // The protothema shape: the page's only reachable video is the promo `<video>`, a single scan result.
        let (_d, repo) = repo();
        on_two_articles(&repo, PROMO);
        let g = guard_scan(&repo, PAGE, None, None, Vec::new(), &s(&[PROMO]), &[]);
        assert!(g.only_furniture, "yt-dlp's attempt must not be made");
        assert_eq!(g.furniture.len(), 1);
        assert_eq!((g.furniture[0].url.as_str(), g.furniture[0].other_articles), (PROMO, 2));
        // Control: a video no other article showed is yt-dlp's to download.
        let (_d2, fresh) = self::repo();
        let g = guard_scan(&fresh, PAGE, None, None, Vec::new(), &s(&[PROMO]), &[]);
        assert!(!g.only_furniture && g.furniture.is_empty());
    }

    #[test]
    fn the_scans_site_is_the_page_yt_dlp_ended_on() {
        let (_d, repo) = repo();
        // The promo was seen on two articles of news.example.
        let key = |p: &str| site_videos::page_key(p, None, None).unwrap();
        for p in ["https://www.news.example/a/1", "https://www.news.example/a/2"] {
            let k = key(p);
            repo.record_site_videos(&k.domain, &k.identities, &site_videos::hashes_of(&s(&[PROMO])), chrono::Utc::now()).unwrap();
        }
        let asked = "https://t.co/AbCdEf";
        assert!(!guard_scan(&repo, asked, None, None, Vec::new(), &s(&[PROMO]), &[]).only_furniture, "t.co is another site");
        assert!(guard_scan(&repo, asked, Some("https://www.news.example/a/3"), None, Vec::new(), &s(&[PROMO]), &[]).only_furniture);
        // A hand-off to a platform names no site: the asked address stays.
        assert!(!guard_scan(&repo, asked, Some("https://www.youtube.com/watch?v=abcdefghijk"), None, Vec::new(), &s(&[PROMO]), &[]).only_furniture);
    }

    #[test]
    fn a_scanned_playlist_loses_its_site_wide_entries_and_ends_only_when_none_is_left() {
        let (_d, repo) = repo();
        on_two_articles(&repo, PROMO);
        let g = guard_scan(&repo, PAGE, None, None, s(&[PROMO, FB_CLIP]), &[], &[]);
        assert_eq!((g.videos.as_slice(), g.only_furniture), (s(&[FB_CLIP]).as_slice(), false));
        assert_eq!(g.furniture.len(), 1);
        let g = guard_scan(&repo, PAGE, None, None, s(&[PROMO]), &[], &[]);
        assert!(g.videos.is_empty() && g.only_furniture);
    }

    /// What `run_job` does after the screen: the photo-only posts leave (`videoless`), the held rule runs, then
    /// `choose_primary` walks what is left with `probe`.
    async fn after_the_filters(screen: &SniffScreen, videoless: &[&str], held: impl Fn(&str) -> bool + Copy, probe: Probe) -> (Vec<String>, Option<String>) {
        let streams: Vec<String> = screen.pool.iter().filter(|u| !videoless.contains(&u.as_str())).cloned().collect();
        let streams = leave_nothing_held(screen.removed_own, streams, held).0;
        let cands = sniffed_cands(&streams, held);
        let primary = match choose_primary(&cands, 3, |_| async move { probe }).await {
            Choice::Primary { url, .. } => Some(url),
            _ => None,
        };
        (streams, primary)
    }

    #[tokio::test]
    async fn furniture_taking_the_articles_own_video_never_hands_the_page_to_a_held_one() {
        // V sits in the article but is the site's furniture; X is an in-article post that turns out to be a photo; W is a rail video.
        let (_d, repo) = repo();
        remember(&repo, OTHER_PAGE, None, &s(&[YT_SHOW]));
        let held = |u: &str| u == FB_CLIP;
        let screen = screen_sniff(&repo, PAGE, None, &s(&[YT_SHOW, X_POST, FB_CLIP]), false, held);
        assert!(screen.removed_own && screen.furniture.len() == 1);
        assert_eq!(screen.pool, s(&[X_POST, FB_CLIP]));
        // X is dropped as videoless: only W is left, and it is never the article's primary.
        let (streams, primary) = after_the_filters(&screen, &[X_POST], held, Probe::Ok).await;
        assert!(streams.is_empty() && primary.is_none(), "{streams:?} {primary:?}");
        assert!(ends_on_furniture(&screen.furniture, &streams), "the job ends on furniture");
        // X is a real video but gone: the own tier is all dead, and W is still not taken.
        let (streams, primary) = after_the_filters(&screen, &[], held, Probe::Dead(omni_broadcast::errors::ErrorCode::PageNotFound)).await;
        assert_eq!(streams, s(&[X_POST, FB_CLIP]));
        assert!(primary.is_none(), "W is never the article's primary: {primary:?}");
        // X plays: it is the primary, W stays behind it.
        let (_, primary) = after_the_filters(&screen, &[], held, Probe::Ok).await;
        assert_eq!(primary.as_deref(), Some(X_POST));
        // Control: a page that had only held candidates keeps today's behaviour.
        let (_d2, fresh) = self::repo();
        let screen = screen_sniff(&fresh, PAGE, None, &s(&[FB_CLIP]), false, held);
        assert!(!screen.removed_own);
        let (streams, primary) = after_the_filters(&screen, &[], held, Probe::Ok).await;
        assert_eq!((streams, primary), (s(&[FB_CLIP]), Some(FB_CLIP.to_string())));
    }

    #[test]
    fn an_article_reached_by_its_short_link_and_by_its_direct_link_is_one_article_in_either_order() {
        for short_first in [true, false] {
            let (_d, repo) = repo();
            let direct = "https://www.news.example/a/story-1";
            let mut via_short = PageSnapshot::new("https://t.co/AbCdEf");
            via_short.final_url = Some(direct.into());
            if short_first {
                remember(&repo, "https://t.co/AbCdEf", Some(&via_short), &s(&[FB_CLIP]));
                remember(&repo, direct, None, &s(&[FB_CLIP]));
            } else {
                remember(&repo, direct, None, &s(&[FB_CLIP]));
                remember(&repo, "https://t.co/AbCdEf", Some(&via_short), &s(&[FB_CLIP]));
            }
            assert!(recall(&repo, direct, None, &s(&[FB_CLIP])).is_empty(), "short_first={short_first}: its own clip is not 'on 2 articles'");
            let screen = screen_sniff(&repo, "https://t.co/AbCdEf", Some(&via_short), &s(&[FB_CLIP]), false, |_| false);
            assert!(screen.furniture.is_empty(), "short_first={short_first}");
        }
    }

    #[test]
    fn a_hand_off_to_a_player_domain_does_not_hide_the_asked_sites_furniture() {
        let (_d, repo) = repo();
        on_two_articles(&repo, PROMO);
        // yt-dlp's scan ended on a player domain; nothing is ever recorded there.
        let g = guard_scan(&repo, PAGE, Some("https://cdn.player.example/players/abc.html"), None, Vec::new(), &s(&[PROMO]), &[]);
        assert!(g.only_furniture, "the asked site is still looked up");
        // The browser's final page is another domain of the same site: looked up too.
        let key = site_videos::page_key("https://www.news.example/a/1", None, None).unwrap();
        repo.record_site_videos(&key.domain, &key.identities, &site_videos::hashes_of(&s(&[FB_CLIP])), chrono::Utc::now()).unwrap();
        let key = site_videos::page_key("https://www.news.example/a/2", None, None).unwrap();
        repo.record_site_videos(&key.domain, &key.identities, &site_videos::hashes_of(&s(&[FB_CLIP])), chrono::Utc::now()).unwrap();
        let asked = "https://t.co/AbCdEf";
        assert!(!guard_scan(&repo, asked, None, None, Vec::new(), &s(&[FB_CLIP]), &[]).only_furniture);
        assert!(guard_scan(&repo, asked, None, Some("https://www.news.example/a/3"), Vec::new(), &s(&[FB_CLIP]), &[]).only_furniture);
        assert!(guard_scan(&repo, asked, Some("https://cdn.player.example/p.html"), Some("https://www.news.example/a/3"), Vec::new(), &s(&[FB_CLIP]), &[]).only_furniture);
    }

    #[test]
    fn a_platform_post_is_not_a_page_of_a_site() {
        let (_d, repo) = repo();
        assert!(remember(&repo, "https://www.youtube.com/watch?v=abcdefghijk", None, &s(&[PROMO])).is_empty());
        let conn = rusqlite::Connection::open(_d.0.join("omni.db")).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
}
