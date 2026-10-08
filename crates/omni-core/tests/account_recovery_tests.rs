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

#[test]
fn a_new_database_gets_admin_admin_once_and_never_again() {
    // Owner's decision (2026-10-08): the first account of a new
    // installation, replaced from the panel. Never on a database in use,
    // never recreated after it was replaced.
    let dir = tempfile::tempdir().unwrap();
    let repo = omni_core::repository::Repository::new(dir.path().join("omni.db")).unwrap();
    assert!(repo.ensure_default_admin().unwrap(), "a brand-new database");
    assert!(repo.has_active_admin().unwrap());
    assert!(repo.default_admin_still_works());
    assert!(!repo.ensure_default_admin().unwrap(), "once");

    // Replaced: its password changed (or the account deactivated).
    let admin = repo.get_user_by_email("Admin").unwrap().expect("matched case-insensitively");
    repo.update_user_password(admin.id, "a-long-real-passphrase").unwrap();
    assert!(!repo.default_admin_still_works(), "the reminder clears");
    repo.set_user_active_status(admin.id, false).unwrap();
    assert!(!repo.ensure_default_admin().unwrap(), "not brought back by a restart");

    // A database that already has its own accounts (this PC's, upgraded).
    let dir2 = tempfile::tempdir().unwrap();
    let used = omni_core::repository::Repository::new(dir2.path().join("omni.db")).unwrap();
    used.create_user("it@station.gr", "a-long-real-passphrase", omni_core::models::UserRole::Admin, "IT", None).unwrap();
    assert!(!used.ensure_default_admin().unwrap());
    assert!(used.get_user_by_email("admin").unwrap().is_none());
}

#[test]
fn the_desks_are_reminded_while_admin_admin_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let repo = omni_core::repository::Repository::new(dir.path().join("omni.db")).unwrap();
    let health = omni_core::health::HealthState::new();
    omni_core::selftest::check_default_admin(&repo, &health);
    assert!(health.get(omni_core::health::checks::ACCOUNTS).is_none(), "no account, nothing to say");
    repo.ensure_default_admin().unwrap();
    omni_core::selftest::check_default_admin(&repo, &health);
    let c = health.get(omni_core::health::checks::ACCOUNTS).expect("reminder");
    assert_eq!(c.state, omni_core::health::Health::Degraded);
    let id = repo.get_user_by_email("admin").unwrap().unwrap().id;
    repo.set_user_active_status(id, false).unwrap();
    omni_core::selftest::check_default_admin(&repo, &health);
    assert!(health.get(omni_core::health::checks::ACCOUNTS).is_none(), "gone once replaced");
}
