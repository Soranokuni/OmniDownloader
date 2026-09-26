//! What the parser would do with a mail, as text for a person (plan P4.16).
//!
//! `omni-ingest mail-preview` reads recent mail (or `.eml` files) and prints
//! this report instead of queueing anything, so parser behaviour can be
//! checked against real traffic without touching the queue or the mailbox.

use std::fmt::Write;

use crate::mail::InboundMail;
use crate::parser::{ParsedEmail, Tier};

fn tier_label(t: Tier) -> &'static str {
    match t {
        Tier::Tier1 => "video platform",
        Tier::Tier2 => "news portal",
        Tier::Locker => "file locker",
        Tier::Image => "image",
        Tier::Other => "unknown site",
        Tier::Attachment => "attachment",
    }
}

/// One mail's report. `seen` is the outcome already recorded for it in
/// `processed_mail`, if any.
pub fn render(mail: &InboundMail, parsed: &ParsedEmail, seen: Option<&str>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "════════════════════════════════════════════════════════════");
    let _ = writeln!(out, "Subject:    {}", mail.subject);
    let _ = writeln!(out, "From:       {} <{}>", mail.from_name, mail.from_address);
    if let Some(at) = mail.received_at {
        let _ = writeln!(out, "Received:   {}", at.format("%Y-%m-%d %H:%M UTC"));
    }
    let _ = writeln!(out, "Message-ID: {}", mail.internet_message_id);
    if let Some(outcome) = seen {
        let _ = writeln!(out, "Status:     already ingested ({outcome}); the daemon will not queue it again");
    }
    let _ = writeln!(
        out,
        "Journalist: {} (found by {:?}){}",
        parsed.journalist.surname,
        parsed.journalist.how,
        if parsed.urgent { "   URGENT" } else { "" }
    );
    let _ = writeln!(out, "Outcome:    {:?}", parsed.outcome);

    for s in &parsed.sections {
        let _ = writeln!(
            out,
            "\n  [{}] {}  → keyword {}",
            s.index_str,
            s.title.as_deref().unwrap_or("(no title)"),
            s.keyword.as_deref().unwrap_or("(none)")
        );
        if s.jobs.is_empty() {
            let _ = writeln!(out, "      (no link)");
        }
        for j in &s.jobs {
            let _ = writeln!(
                out,
                "      {}_{}_{}  {}  [{}, {}, confidence {:.2}{}]",
                j.index_str,
                parsed.journalist.surname,
                j.keyword,
                j.url,
                tier_label(j.tier),
                j.status.as_str(),
                j.confidence,
                if j.marker { ", marked" } else { "" }
            );
        }
    }
    if !parsed.ignored_urls.is_empty() {
        let _ = writeln!(out, "\n  Links seen but not queued:");
        for u in &parsed.ignored_urls {
            let _ = writeln!(out, "      {u}");
        }
    }
    if !parsed.warnings.is_empty() {
        let _ = writeln!(out, "\n  Warnings:");
        for w in &parsed.warnings {
            let _ = writeln!(out, "      {}{}", w.code, w.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default());
        }
    }
    out
}
