//! The mail-preview report (plan P4.16): what an operator reads to check
//! the parser against real mail. Names and addresses are invented.

use std::path::Path;

use omni_core::models::Journalist;
use omni_email::mail::InboundMail;
use omni_email::parser::{parse, ParserConfig};
use omni_email::preview::render;

fn roster() -> Vec<Journalist> {
    vec![Journalist {
        id: 0,
        surname: "NIKOLAOU".into(),
        full_name: "Giorgos Nikolaou".into(),
        emails: vec!["g.nikolaou@example.gr".into()],
        default_priority: 0,
        aliases: vec![],
        groups: vec![],
        created_at: None,
    }]
}

#[test]
fn the_report_names_slugs_links_statuses_and_what_was_left_out() {
    let raw = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/17_outlook_html_hyperlinks_and_reply.eml"),
    )
    .unwrap();
    let mail = InboundMail::from_rfc822("fixture", &raw).unwrap();
    let parsed = parse(&mail, &roster(), &ParserConfig::default());
    let text = render(&mail, &parsed, None);

    assert!(text.contains("Journalist: NIKOLAOU (found by Sender)"), "{text}");
    assert!(text.contains("[1] ΣΕΙΣΜΟΣ ΣΤΗ ΣΗΤΕΙΑ  → keyword SEISMOSSITEIA"), "{text}");
    assert!(
        text.contains("1_NIKOLAOU_SEISMOSSITEIA  https://www.youtube.com/watch?v=fixture1701  [video platform, PENDING"),
        "{text}"
    );
    assert!(text.contains("Links seen but not queued:\n      https://www.example.gr/"), "{text}");
    assert!(!text.contains("already ingested"));

    let again = render(&mail, &parsed, Some("JOBS"));
    assert!(again.contains("already ingested (JOBS)"), "{again}");
}
