//! Regression tests for defect D-11 (fabricated timestamps), the session
//! expiry comparison, and the migration framework (plan P0.2 / P0.3).

use std::thread::sleep;
use std::time::Duration;

use omni_core::models::JobStatus;
use omni_core::repository::Repository;

fn temp_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

#[test]
fn stored_timestamps_are_real_and_advance_on_update() {
    let (_dir, repo) = temp_repo();

    let id = repo
        .add_job(
            "https://www.youtube.com/watch?v=abc123",
            "1_MCR_TEST",
            "MCR",
            "TEST",
            "1",
            0,
            JobStatus::Pending,
            None,
            None,
            None,
        )
        .unwrap();

    let created = repo
        .get_job(id)
        .unwrap()
        .expect("job must exist")
        .created_at
        .expect("created_at must be a real stored value, not NULL");

    // The old code returned Utc::now() on every read, so created_at and
    // updated_at were always equal and always "now". Sleep past the storage
    // resolution (milliseconds) by a clear margin.
    sleep(Duration::from_millis(1100));
    repo.update_job_status(id, JobStatus::Completed, None, Some("out.mxf"), Some(12.0))
        .unwrap();

    let job = repo.get_job(id).unwrap().unwrap();
    let updated = job.updated_at.expect("updated_at must be stored");

    assert!(
        updated > created,
        "updated_at ({updated}) must be later than created_at ({created}); \
         if they are equal the read is fabricating the value again (D-11)"
    );
    // And the value must be the *stored* time, not the time of this read.
    let now = chrono::Utc::now();
    assert!(
        (now - created).num_milliseconds() >= 1000,
        "created_at came back as approximately now ({created} vs {now}); it is \
         still being substituted on read"
    );
}

#[test]
fn an_expired_session_does_not_authenticate() {
    let (_dir, repo) = temp_repo();

    let user_id = repo
        .create_user(
            "operator@example.gr",
            "correct horse battery staple",
            omni_core::models::UserRole::User,
            "Operator",
            None,
        )
        .unwrap();

    // A session that expired one minute ago -- i.e. earlier the *same day*,
    // which is the case the old code got wrong. It stored expires_at as
    // to_rfc3339() ("2026-09-19T16:00:00+00:00") and compared it against
    // CURRENT_TIMESTAMP ("2026-09-19 17:00:00"); SQLite compares those as text,
    // and 'T' (0x54) sorts above ' ' (0x20), so the expired session still
    // authenticated. A whole-day expiry would have been rejected even then, so
    // testing with -1 day would not catch the regression.
    repo.create_session_for(user_id, "expired-token", chrono::Duration::minutes(-1))
        .unwrap();
    assert!(
        repo.get_user_by_session_token("expired-token")
            .unwrap()
            .is_none(),
        "a session that expired a minute ago still authenticated a user"
    );

    // Yesterday's session, for good measure.
    repo.create_session(user_id, "stale-token", -1).unwrap();
    assert!(repo
        .get_user_by_session_token("stale-token")
        .unwrap()
        .is_none());

    repo.create_session(user_id, "live-token", 1).unwrap();
    assert!(
        repo.get_user_by_session_token("live-token")
            .unwrap()
            .is_some(),
        "a valid session token failed to authenticate"
    );
}

#[test]
fn reopening_a_database_preserves_jobs_and_does_not_remigrate() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("omni.db");

    let id = {
        let repo = Repository::new(&db).unwrap();
        repo.add_job(
            "https://www.lifo.gr/story",
            "3_PAPADAKI_KNICKS",
            "PAPADAKI",
            "KNICKS",
            "3",
            0,
            JobStatus::Pending,
            None,
            None,
            None,
        )
        .unwrap()
    };

    // Second open runs the migration framework again; it must find itself
    // already at the target version and leave data alone.
    let repo = Repository::new(&db).unwrap();
    let job = repo.get_job(id).unwrap().expect("job survived reopen");
    assert_eq!(job.slug, "3_PAPADAKI_KNICKS");
    assert!(job.created_at.is_some());
}

#[test]
fn an_account_created_the_way_the_wizard_creates_it_can_log_in() {
    // Regression for W-05: the setup wizard hashed the password and then passed
    // the hash to create_user, which hashed it again. The wizard reported
    // success and the operator discovered the broken account at the login
    // screen. create_user takes plaintext and is the only place that hashes.
    let (_dir, repo) = temp_repo();

    let password = "a-long-enough-admin-password";
    repo.create_user(
        "admin@example.gr",
        password,
        omni_core::models::UserRole::Admin,
        "System Administrator",
        None,
    )
    .unwrap();

    let user = repo
        .get_user_by_email("admin@example.gr")
        .unwrap()
        .expect("admin must exist");
    assert!(
        omni_core::auth::verify_password(password, &user.password_hash),
        "the password the wizard collected does not verify against the stored hash"
    );
}
