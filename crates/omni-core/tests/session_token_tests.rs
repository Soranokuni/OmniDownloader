//! P2.19: session bearer tokens are stored as a hash, and the per-request touch
//! writes at most once a minute.

use chrono::{Duration, Utc};
use omni_core::models::UserRole;
use omni_core::repository::{session_key, Repository};
use omni_core::timestamps;
use rusqlite::Connection;

const TOKEN: &str = "f3a9c1d7e5b24680aa11bb22cc33dd44ee55ff66778899aabbccddeeff001122";

struct Fx {
    _dir: tempfile::TempDir,
    repo: Repository,
    raw: Connection,
    uid: i64,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("omni.db");
    let repo = Repository::new(&path).unwrap();
    let raw = Connection::open(&path).unwrap();
    let uid = repo
        .create_user("sess@example.invalid", "correct horse battery", UserRole::Admin, "Sess Tester", None)
        .unwrap();
    Fx { _dir: dir, repo, raw, uid }
}

fn count(f: &Fx) -> i64 {
    f.raw.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)).unwrap()
}

fn last_seen(f: &Fx) -> String {
    f.raw.query_row("SELECT last_seen_at FROM sessions", [], |r| r.get(0)).unwrap()
}

#[test]
fn the_database_never_holds_the_cookie_token() {
    let f = fx();
    f.repo
        .create_session_with_meta(f.uid, TOKEN, Duration::days(1), Some("10.0.0.5"), Some("test-agent"))
        .unwrap();
    let (key, cols): (String, Vec<String>) = {
        let mut stmt = f.raw.prepare("SELECT * FROM sessions").unwrap();
        let n = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        let row = rows.next().unwrap().expect("one session row");
        let cols = (0..n)
            .map(|i| match row.get_ref(i).unwrap() {
                rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                other => format!("{other:?}"),
            })
            .collect();
        (row.get("token").unwrap(), cols)
    };
    assert_eq!(key, session_key(TOKEN));
    assert_eq!(key.len(), 64);
    assert_ne!(key, TOKEN);
    for value in &cols {
        for start in 0..=(TOKEN.len() - 16) {
            assert!(!value.contains(&TOKEN[start..start + 16]), "a piece of the token is stored: {value}");
        }
    }
}

#[test]
fn lookup_touch_and_every_way_of_ending_a_session_work_through_the_hash() {
    let f = fx();
    f.repo.create_session(f.uid, TOKEN, 1).unwrap();
    assert_eq!(f.repo.get_user_by_session_token(TOKEN).unwrap().map(|u| u.id), Some(f.uid));
    // The stored key is not itself a credential.
    assert!(f.repo.get_user_by_session_token(&session_key(TOKEN)).unwrap().is_none());
    assert!(f.repo.touch_session(TOKEN, Duration::hours(1)).unwrap());

    f.repo.delete_session(TOKEN).unwrap();
    assert_eq!(count(&f), 0, "logout left the row");
    assert!(f.repo.get_user_by_session_token(TOKEN).unwrap().is_none());

    f.repo.create_session(f.uid, TOKEN, 1).unwrap();
    f.repo.create_session(f.uid, "second-token", 1).unwrap();
    assert_eq!(f.repo.delete_sessions_for_user(f.uid).unwrap(), 2);

    f.repo.create_session(f.uid, TOKEN, 1).unwrap();
    f.repo.update_user_password(f.uid, "another long passphrase").unwrap();
    assert_eq!(count(&f), 0, "a password change left the session alive");
    assert!(f.repo.get_user_by_session_token(TOKEN).unwrap().is_none());
}

#[test]
fn touch_writes_at_most_once_a_minute() {
    let f = fx();
    let idle = Duration::hours(8);
    f.repo.create_session(f.uid, TOKEN, 1).unwrap();
    f.repo.backdate_session_last_seen(TOKEN, Duration::minutes(5)).unwrap();
    let start = Utc::now();

    let mut seen = vec![last_seen(&f)];
    for i in 0..10 {
        assert!(f.repo.touch_session_at(TOKEN, idle, start + Duration::seconds(i * 5)).unwrap());
        seen.push(last_seen(&f));
    }
    seen.dedup();
    assert_eq!(seen.len(), 2, "10 touches inside a minute must write once: {seen:?}");
    assert_eq!(seen[1], timestamps::format(start));

    // Past a minute since the last write, it writes again.
    let later = start + Duration::seconds(61);
    assert!(f.repo.touch_session_at(TOKEN, idle, later).unwrap());
    assert_eq!(last_seen(&f), timestamps::format(later));
}

#[test]
fn a_clock_stepped_back_rewrites_a_last_seen_in_the_future() {
    let f = fx();
    let idle = Duration::hours(8);
    f.repo.create_session(f.uid, TOKEN, 1).unwrap();
    let seen = Utc::now();
    assert!(f.repo.touch_session_at(TOKEN, idle, seen + Duration::seconds(61)).unwrap());
    // The clock steps back 10 minutes: the stored value is now in the future.
    let back = seen - Duration::minutes(10);
    assert!(f.repo.touch_session_at(TOKEN, idle, back).unwrap());
    assert_eq!(last_seen(&f), timestamps::format(back));
}

#[test]
fn idle_expiry_is_checked_on_every_touch() {
    let f = fx();
    f.repo.create_session(f.uid, TOKEN, 1).unwrap();
    // Seen 10 s ago: inside the write throttle, but idle limit is 5 s.
    f.repo.backdate_session_last_seen(TOKEN, Duration::seconds(10)).unwrap();
    assert!(!f.repo.touch_session_at(TOKEN, Duration::seconds(5), Utc::now()).unwrap());
    assert_eq!(count(&f), 0, "an idle session was not deleted");
}
