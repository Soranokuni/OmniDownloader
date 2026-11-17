//! Self-check link bookkeeping (plan P6.7).

use omni_core::repository::Repository;

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

#[test]
fn a_fresh_database_has_one_link_per_route_and_none_is_checked_yet() {
    let (_d, repo) = repo();
    let links = repo.list_selfcheck_links().unwrap();
    assert!(links.len() >= 8, "{links:?}");
    assert!(links.iter().all(|l| l.last_ok.is_none() && l.failing_since.is_none()));
    for kind in ["YouTube", "Instagram", "Facebook", "TikTok", "News article"] {
        assert!(links.iter().any(|l| l.label.starts_with(kind)), "no {kind} link");
    }
}

#[test]
fn failing_since_is_the_first_failure_and_clears_when_it_works_again() {
    let (_d, repo) = repo();
    let id = repo.add_selfcheck_link("Test", "https://example.org/video/1").unwrap();
    assert!(repo.add_selfcheck_link("Again", "https://example.org/video/1").is_err(), "a duplicate link is refused");

    let get = || repo.list_selfcheck_links().unwrap().into_iter().find(|l| l.id == id).unwrap();

    assert_eq!(repo.record_selfcheck_result(id, true, "found x").unwrap(), None);
    assert_eq!(get().last_ok, Some(true));
    assert!(get().failing_since.is_none());

    assert_eq!(repo.record_selfcheck_result(id, false, "gone").unwrap(), Some(true));
    let since = get().failing_since.expect("failing since set");
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(repo.record_selfcheck_result(id, false, "still gone").unwrap(), Some(false));
    let l = get();
    assert_eq!(l.failing_since, Some(since), "a second failure keeps the first date");
    assert_eq!(l.last_detail.as_deref(), Some("still gone"));
    assert!(l.last_ok_at.is_some(), "the last success is remembered");

    repo.record_selfcheck_result(id, true, "back").unwrap();
    assert!(get().failing_since.is_none());

    assert!(repo.delete_selfcheck_link(id).unwrap());
    assert!(!repo.delete_selfcheck_link(id).unwrap());
}

/// P11.4: a stored result is put back exactly as it was before, so a yt-dlp
/// build that is not left live leaves no trace in the self-check.
#[test]
fn a_stored_result_is_put_back_exactly_as_it_was() {
    let (_d, repo) = repo();
    let id = repo.add_selfcheck_link("Test", "https://example.org/video/2").unwrap();
    let get = || repo.list_selfcheck_links().unwrap().into_iter().find(|l| l.id == id).unwrap();
    repo.record_selfcheck_result(id, true, "found x").unwrap();
    let before = get();
    repo.record_selfcheck_result(id, false, "broken by the new build").unwrap();
    assert!(get().failing_since.is_some());
    assert!(repo.restore_selfcheck_result(&before).unwrap());
    let after = get();
    assert_eq!(
        (after.last_ok, after.failing_since, after.last_detail.clone(), after.last_checked_at, after.last_ok_at),
        (before.last_ok, before.failing_since, before.last_detail.clone(), before.last_checked_at, before.last_ok_at)
    );
    // A link never checked is put back to never checked.
    let fresh = repo.add_selfcheck_link("Fresh", "https://example.org/video/3").unwrap();
    let untouched = repo.list_selfcheck_links().unwrap().into_iter().find(|l| l.id == fresh).unwrap();
    repo.record_selfcheck_result(fresh, false, "x").unwrap();
    repo.restore_selfcheck_result(&untouched).unwrap();
    let back = repo.list_selfcheck_links().unwrap().into_iter().find(|l| l.id == fresh).unwrap();
    assert_eq!((back.last_ok, back.last_checked_at, back.failing_since), (None, None, None));
}
