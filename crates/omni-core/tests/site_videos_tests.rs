//! Site furniture memory (P3.16): which videos were seen on which article of a
//! site, and which of them are the site's own. Hashes only; invented names.

use chrono::{Duration, Utc};
use tempfile::TempDir;

use omni_core::repository::Repository;
use omni_core::site_videos::{page_key, video_hash, SITE_WIDE_MIN_ARTICLES, SITE_WIDE_WINDOW_DAYS};

fn repo(dir: &TempDir) -> Repository {
    Repository::new(dir.path().join("omni.db")).unwrap()
}

fn promo() -> Vec<String> {
    vec![video_hash("https://media.news.example/promo-hor.mp4")]
}

fn since(now: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    omni_core::site_videos::window_start(now)
}

#[test]
fn the_migration_creates_the_table_and_the_version_is_recorded() {
    let dir = TempDir::new().unwrap();
    let _ = repo(&dir);
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
    assert_eq!(omni_core::migrations::current_version(&conn).unwrap(), omni_core::migrations::target_version());
    let idx: i64 = conn
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_site_videos_seen'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(idx, 1);
}

#[test]
fn one_article_is_not_site_wide_two_are() {
    assert_eq!((SITE_WIDE_MIN_ARTICLES, SITE_WIDE_WINDOW_DAYS), (2, 7));
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    repo.record_site_videos("news.example", &["page-a".to_string()], &promo(), now).unwrap();
    assert!(repo.site_wide("news.example", &promo(), since(now), &[]).unwrap().is_empty(), "a first sighting is not furniture");
    repo.record_site_videos("news.example", &["page-b".to_string()], &promo(), now).unwrap();
    let wide = repo.site_wide("news.example", &promo(), since(now), &[]).unwrap();
    assert_eq!(wide.get(&promo()[0]), Some(&2));
}

#[test]
fn the_same_article_seen_twice_counts_once() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    repo.record_site_videos("news.example", &["page-a".to_string()], &promo(), now - Duration::days(2)).unwrap();
    repo.record_site_videos("news.example", &["page-a".to_string()], &promo(), now).unwrap();
    assert!(repo.site_wide("news.example", &promo(), since(now), &[]).unwrap().is_empty());
    // The page being asked about is counted once, whether stored or not.
    assert!(repo.site_wide("news.example", &promo(), since(now), &["page-a".to_string()]).unwrap().is_empty());
    let wide = repo.site_wide("news.example", &promo(), since(now), &["page-b".to_string()]).unwrap();
    assert_eq!(wide.get(&promo()[0]), Some(&2), "an unstored current page adds itself");
    // Seeing it again moved seen_at: still one row.
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn a_sighting_older_than_the_window_does_not_count() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    repo.record_site_videos("news.example", &["page-a".to_string()], &promo(), now - Duration::days(8)).unwrap();
    repo.record_site_videos("news.example", &["page-b".to_string()], &promo(), now).unwrap();
    assert!(repo.site_wide("news.example", &promo(), since(now), &[]).unwrap().is_empty(), "8 days old is outside the week");
    repo.record_site_videos("news.example", &["page-c".to_string()], &promo(), now - Duration::days(6)).unwrap();
    assert_eq!(repo.site_wide("news.example", &promo(), since(now), &[]).unwrap().get(&promo()[0]), Some(&2), "6 days old counts");
}

#[test]
fn a_site_does_not_see_another_sites_sightings() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    repo.record_site_videos("news.example", &["page-a".to_string()], &promo(), now).unwrap();
    repo.record_site_videos("other.example", &["page-b".to_string()], &promo(), now).unwrap();
    assert!(repo.site_wide("news.example", &promo(), since(now), &[]).unwrap().is_empty());
    assert!(repo.site_wide("other.example", &promo(), since(now), &[]).unwrap().is_empty());
}

#[test]
fn purge_removes_what_is_older_and_keeps_the_rest() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    repo.record_site_videos("news.example", &["old".to_string()], &promo(), now - Duration::days(15)).unwrap();
    repo.record_site_videos("news.example", &["new".to_string()], &promo(), now - Duration::days(1)).unwrap();
    let n = repo.purge_site_videos(now - Duration::days(omni_core::site_videos::SITE_VIDEOS_KEEP_DAYS)).unwrap();
    assert_eq!(n, 1);
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    let pages: Vec<String> = conn
        .prepare("SELECT article_id FROM site_videos")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(pages, vec!["new".to_string()]);
}

#[test]
fn only_hashes_reach_the_database() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let url = "https://cdn.news.example/v/clip.m3u8?token=SECRETVALUE";
    let key = page_key("https://www.news.example/a/story", None, None).unwrap();
    repo.record_site_videos(&key.domain, &key.identities, &[video_hash(url)], Utc::now()).unwrap();
    let bytes = std::fs::read(dir.path().join("omni.db")).unwrap();
    let wal = std::fs::read(dir.path().join("omni.db-wal")).unwrap_or_default();
    for blob in [bytes, wal] {
        assert!(!blob.windows(11).any(|w| w == b"SECRETVALUE"), "no token in the file");
        assert!(!blob.windows(9).any(|w| w == b"clip.m3u8"), "no address in the file");
    }
}

#[test]
fn every_identity_of_the_current_article_is_the_same_article() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    // Stored under the address asked; the browser later ended on another address of the same article.
    repo.record_site_videos("news.example", &["asked".to_string()], &promo(), now).unwrap();
    let ids = vec!["asked".to_string(), "ended-on".to_string()];
    assert!(repo.site_wide("news.example", &promo(), since(now), &ids).unwrap().is_empty(), "one article, however many addresses");
    // Stored under what is now only a second identity: still not another article.
    let ids = vec!["ended-on".to_string(), "asked".to_string()];
    assert!(repo.site_wide("news.example", &promo(), since(now), &ids).unwrap().is_empty());
    // A different article stored too: furniture.
    repo.record_site_videos("news.example", &["elsewhere".to_string()], &promo(), now).unwrap();
    assert_eq!(repo.site_wide("news.example", &promo(), since(now), &ids).unwrap().get(&promo()[0]), Some(&2));
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn count(dir: &TempDir, table: &str) -> i64 {
    rusqlite::Connection::open(dir.path().join("omni.db")).unwrap().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
}

#[test]
fn the_migration_creates_the_alias_table_too() {
    let dir = TempDir::new().unwrap();
    let _ = repo(&dir);
    assert_eq!(count(&dir, "site_video_pages"), 0);
}

#[test]
fn an_article_reached_by_a_short_link_first_and_its_direct_link_later_is_one_article() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    // First through t.co: asked = short, ended on = direct.
    repo.record_site_videos("news.example", &ids(&["short", "direct"]), &promo(), now).unwrap();
    // Later the direct link itself.
    repo.record_site_videos("news.example", &ids(&["direct"]), &promo(), now).unwrap();
    assert_eq!(count(&dir, "site_videos"), 1, "one article, one sighting");
    assert!(repo.site_wide("news.example", &promo(), since(now), &ids(&["direct"])).unwrap().is_empty());
    assert!(repo.site_wide("news.example", &promo(), since(now), &ids(&["short", "direct"])).unwrap().is_empty());
}

#[test]
fn an_article_reached_by_its_direct_link_first_and_a_short_link_later_is_one_article() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    repo.record_site_videos("news.example", &ids(&["direct"]), &promo(), now).unwrap();
    repo.record_site_videos("news.example", &ids(&["short", "direct"]), &promo(), now).unwrap();
    assert_eq!(count(&dir, "site_videos"), 1);
    assert!(repo.site_wide("news.example", &promo(), since(now), &ids(&["short"])).unwrap().is_empty(), "the short link is the same article");
    // A different article is a second one.
    repo.record_site_videos("news.example", &ids(&["other"]), &promo(), now).unwrap();
    assert_eq!(repo.site_wide("news.example", &promo(), since(now), &ids(&["short"])).unwrap().get(&promo()[0]), Some(&2));
}

#[test]
fn two_articles_stored_apart_are_merged_when_one_page_shows_both_were_the_same() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    repo.record_site_videos("news.example", &ids(&["short"]), &promo(), now).unwrap();
    repo.record_site_videos("news.example", &ids(&["direct"]), &promo(), now).unwrap();
    assert_eq!(repo.site_wide("news.example", &promo(), since(now), &ids(&["x"])).unwrap().get(&promo()[0]), Some(&3));
    repo.record_site_videos("news.example", &ids(&["short", "direct"]), &promo(), now).unwrap();
    assert_eq!(count(&dir, "site_videos"), 1);
    assert!(repo.site_wide("news.example", &promo(), since(now), &ids(&["short"])).unwrap().is_empty());
}

#[test]
fn purge_takes_the_aliases_of_articles_with_no_sighting_left() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    repo.record_site_videos("news.example", &ids(&["old-short", "old"]), &promo(), now - Duration::days(15)).unwrap();
    repo.record_site_videos("news.example", &ids(&["new"]), &promo(), now).unwrap();
    assert_eq!(repo.purge_site_videos(now - Duration::days(14)).unwrap(), 1);
    assert_eq!(count(&dir, "site_videos"), 1);
    assert_eq!(count(&dir, "site_video_pages"), 1, "only the live article's alias is left");
}

#[test]
fn merging_two_articles_keeps_the_newest_sighting_of_a_video_both_saw() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    // Both stored apart: the short link saw the promo 6 days ago, the direct link just now.
    repo.record_site_videos("news.example", &ids(&["short"]), &promo(), now - Duration::days(6)).unwrap();
    repo.record_site_videos("news.example", &ids(&["direct"]), &promo(), now).unwrap();
    // A visit that shows both are one article (with another video, so the merge is not a no-op).
    repo.record_site_videos("news.example", &ids(&["short", "direct"]), &[video_hash("https://x.example/b.mp4")], now - Duration::days(1)).unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    let seen: String = conn
        .query_row("SELECT seen_at FROM site_videos WHERE video_hash = ?", [&promo()[0]], |r| r.get(0))
        .unwrap();
    assert_eq!(seen, omni_core::timestamps::format(now), "the newer sighting survived the merge");
    assert_eq!(count(&dir, "site_videos"), 2, "the promo once, the other video once");
}

#[test]
fn merging_never_makes_a_newer_sighting_older() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let now = Utc::now();
    // The reverse of the case above: the surviving article has the newer sighting.
    repo.record_site_videos("news.example", &ids(&["short"]), &promo(), now).unwrap();
    repo.record_site_videos("news.example", &ids(&["direct"]), &promo(), now - Duration::days(6)).unwrap();
    repo.record_site_videos("news.example", &ids(&["short", "direct"]), &[video_hash("https://x.example/b.mp4")], now - Duration::days(1)).unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
    let seen: String = conn
        .query_row("SELECT seen_at FROM site_videos WHERE video_hash = ?", [&promo()[0]], |r| r.get(0))
        .unwrap();
    assert_eq!(seen, omni_core::timestamps::format(now), "the merged row stays at the newer time");
}
