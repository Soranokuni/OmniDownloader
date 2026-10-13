//! Cookie jars (plan P3.5): sealed at rest, replaced on upload, listed without
//! contents, at most 20. Invented domains and values.

use anyhow::Result;
use chrono::{Duration, Utc};
use tempfile::TempDir;

use omni_core::cookies::{parse_netscape, MAX_JARS};
use omni_core::repository::Repository;

const VALUE: &str = "jar-value-0123456789";

fn repo(dir: &TempDir) -> Repository {
    Repository::new(dir.path().join("omni.db")).unwrap()
}

fn jar(domain: &str, value: &str) -> omni_core::cookies::ParsedJar {
    parse_netscape(domain, &format!(".{domain}\tTRUE\t/\tTRUE\t1893456000\tsid\t{value}\n")).unwrap()
}

#[test]
fn a_jar_round_trips_and_its_value_is_not_in_the_database_file() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    r.put_cookie_jar("x.com", &jar("x.com", VALUE), "admin", Utc::now())?;
    assert!(r.cookie_jar_text("x.com")?.unwrap().contains(VALUE));
    assert_eq!(r.cookie_jar_text("facebook.com")?, None);
    assert!(r.has_cookie_jar("x.com")?);

    // Checkpoint is not needed: SQLite stores blobs verbatim in the file or its WAL.
    for entry in std::fs::read_dir(dir.path())? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(path)?;
        assert!(!String::from_utf8_lossy(&bytes).contains(VALUE), "the cookie value is in plaintext on disk");
    }
    assert_eq!(r.cookie_values(), vec![VALUE.to_string()]);
    Ok(())
}

#[test]
fn the_list_has_no_contents_and_an_upload_replaces_and_resets_the_test() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let now = Utc::now();
    r.put_cookie_jar("x.com", &jar("x.com", VALUE), "alice", now)?;
    r.record_cookie_test("x.com", false, "ERROR: login required", "https://x.com/a/status/1?s=20", now)?;
    let l = r.list_cookie_jars()?;
    assert_eq!(l.len(), 1);
    assert_eq!((l[0].cookie_count, l[0].last_test_ok, l[0].updated_by.as_str()), (1, Some(false), "alice"));
    assert!(!format!("{l:?}").contains(VALUE));

    r.put_cookie_jar("x.com", &jar("x.com", "second-value-987654"), "bob", now + Duration::minutes(1))?;
    let l = r.list_cookie_jars()?;
    assert_eq!(l.len(), 1, "a replacement, not a second jar");
    assert_eq!((l[0].last_test_ok, l[0].updated_by.as_str()), (None, "bob"), "the old test described other cookies");
    assert_eq!(l[0].last_sample_url.as_deref(), Some("https://x.com/a/status/1?s=20"), "the sample address is kept for the nightly test");
    assert!(r.cookie_jar_text("x.com")?.unwrap().contains("second-value-987654"));
    assert!(!r.cookie_jar_text("x.com")?.unwrap().contains(VALUE));

    assert!(r.delete_cookie_jar("x.com")?);
    assert!(!r.delete_cookie_jar("x.com")?);
    assert!(r.list_cookie_jars()?.is_empty());
    Ok(())
}

#[test]
fn at_most_twenty_jars_but_a_full_store_still_takes_a_replacement() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    for i in 0..MAX_JARS {
        let d = format!("site{i}.example");
        r.put_cookie_jar(&d, &jar(&d, VALUE), "admin", Utc::now())?;
    }
    assert!(r.put_cookie_jar("onemore.example", &jar("onemore.example", VALUE), "admin", Utc::now()).is_err());
    r.put_cookie_jar("site3.example", &jar("site3.example", "replacement-value-1"), "admin", Utc::now())?;
    assert_eq!(r.list_cookie_jars()?.len(), MAX_JARS);
    Ok(())
}
