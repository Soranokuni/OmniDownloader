//! `omni-ingest admin reset-password` (2026-09-30) rests on
//! `update_user_password`: the old password stops working, the new one
//! works, and every session of the account ends. Invented addresses only.

use omni_core::auth::{validate_password, verify_password};
use omni_core::models::UserRole;
use omni_core::repository::Repository;

#[test]
fn a_reset_password_replaces_the_old_one_and_ends_every_session() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    let id = repo
        .create_user("admin@newsroom.test", "the-old-password-1", UserRole::Admin, "Admin", None)
        .unwrap();
    repo.create_session(id, "token-a", 1).unwrap();
    repo.create_session(id, "token-b", 1).unwrap();

    repo.update_user_password(id, "the-new-password-2").unwrap();

    let user = repo.get_user_by_email("ADMIN@newsroom.test").unwrap().expect("found case-insensitively");
    let hash = {
        let conn = rusqlite::Connection::open(dir.path().join("omni.db")).unwrap();
        conn.query_row("SELECT password_hash FROM users WHERE id = ?", [user.id], |r| r.get::<_, String>(0))
            .unwrap()
    };
    assert!(verify_password("the-new-password-2", &hash));
    assert!(!verify_password("the-old-password-1", &hash));
    assert!(repo.delete_sessions_for_user(id).unwrap() == 0, "sessions survived the reset");

    assert!(validate_password("short").is_err());
    assert!(validate_password("twelve-chars").is_ok());
}
