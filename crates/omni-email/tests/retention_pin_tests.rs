//! P1.21: the retention of handled-mail records must outlast what the watcher ingests.

use omni_core::repository::MAIL_KEEP_DAYS;
use omni_email::watcher::{MAIL_RECORD_MARGIN_DAYS, MAX_MAIL_AGE};

#[test]
fn mail_records_are_kept_longer_than_the_watcher_accepts_old_mail() {
    // A record purged while its mail could still be accepted would let that mail in again.
    let safe_window = chrono::Duration::days(MAIL_KEEP_DAYS - MAIL_RECORD_MARGIN_DAYS);
    assert!(MAIL_RECORD_MARGIN_DAYS >= 7, "a week of margin between ingest window and purge");
    assert!(
        MAX_MAIL_AGE < safe_window,
        "MAX_MAIL_AGE ({MAX_MAIL_AGE}) must be below MAIL_KEEP_DAYS - margin ({safe_window})"
    );
}
