//! The MCR mail view (plan P7.7): what the desk shows of a mail, which link
//! became which job, and why the others became none.
//!
//! All names and addresses are invented (the repository is public).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use omni_core::models::{Enqueued, Job, JobStatus, Journalist, NewJob, ProcessedMail};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use omni_email::mail::InboundMail;
use omni_email::mail_view::{self, build, MailParseSummary, NoteLevel, Segment};
use omni_email::parser::{parse, view_lines, LineRole, ParserConfig};
use omni_email::watcher::QueuedFromMail;

fn roster() -> Vec<Journalist> {
    let j = |surname: &str, full: &str, emails: &[&str], aliases: &[&str]| Journalist {
        id: 0,
        surname: surname.into(),
        full_name: full.into(),
        emails: emails.iter().map(|s| s.to_string()).collect(),
        default_priority: 0,
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
        groups: vec![],
        created_at: None,
    };
    vec![
        j("MCR", "Master Control Room", &["master@example.gr", "flow@example.gr"], &[]),
        j("PAPADAKI", "Anna Papadaki", &["a.papadaki@example.gr"], &["ΠΑΠΑΔΑΚΗ", "ΑΝΝΑ"]),
        j("NIKOLAOU", "Giorgos Nikolaou", &["g.nikolaou@example.gr"], &["ΝΙΚΟΛΑΟΥ", "ΓΙΩΡΓΟΣ"]),
        j("GEORGIOU", "Eleni Georgiou", &["e.georgiou@example.gr"], &["ΓΕΩΡΓΙΟΥ", "ΕΛΕΝΗ"]),
        j("DIMITRIOU", "Kostas Dimitriou", &["k.dimitriou@example.gr"], &["ΔΗΜΗΤΡΙΟΥ", "ΚΩΣΤΑ"]),
    ]
}

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

/// Queue `mail` the way the watcher does and return what the database then
/// holds: the processed_mail row and the mail's jobs.
fn ingest(repo: &Repository, mail: &InboundMail) -> (ProcessedMail, Vec<Job>) {
    let parsed = parse(mail, &roster(), &ParserConfig::default());
    let key = omni_email::watcher::mail_key(mail);
    let mut records = Vec::new();
    for job in parsed.jobs() {
        let slug = format!("{}_{}_{}", job.index_str, parsed.journalist.surname, job.keyword);
        let mut new = NewJob::new(job.url.clone(), slug.clone(), parsed.journalist.surname.clone());
        new.keyword = job.keyword.clone();
        new.index_str = job.index_str.clone();
        new.email_message_id = Some(key.clone());
        let result = repo.enqueue(&new, DEFAULT_DEDUP_WINDOW_HOURS).unwrap();
        records.push(QueuedFromMail {
            index_str: job.index_str.clone(),
            slug,
            url: job.url.clone(),
            status: "PENDING".into(),
            result,
            attachment_id: job.attachment_id.clone(),
        });
    }
    let row = ProcessedMail {
        internet_message_id: key.clone(),
        outcome: serde_json::to_value(parsed.outcome).unwrap().as_str().unwrap().into(),
        subject: Some(mail.subject.clone()),
        jobs_json: serde_json::to_string(&records).unwrap(),
        body_text: Some(mail_view::stored_text(&mail.readable_body())),
        attachments_json: Some(serde_json::to_string(&mail.attachments).unwrap()),
        parse_json: Some(serde_json::to_string(&MailParseSummary::of(&parsed)).unwrap()),
        ..Default::default()
    };
    let jobs = repo.jobs_for_mail(&key, &[]).unwrap();
    (row, jobs)
}

fn mail(subject: &str, body: &str) -> InboundMail {
    InboundMail {
        id: "m1".into(),
        internet_message_id: "<m1@example.gr>".into(),
        from_address: "e.georgiou@example.gr".into(),
        subject: subject.into(),
        body_text: body.into(),
        ..Default::default()
    }
}

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "eml"))
        .collect();
    v.sort();
    v
}

/// The whole text of a view, as the desk shows it.
fn shown(view: &mail_view::MailView) -> String {
    view.text
        .iter()
        .flatten()
        .flat_map(|b| b.lines.iter())
        .map(|line| {
            line.iter()
                .map(|s| match s {
                    Segment::Text { t } | Segment::Link { t, .. } => t.as_str(),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The drift guard: whatever the parser queued from a mail's text, the view
/// shows as a link in that text, filed with its job. A parser change that
/// finds links the view cannot see (or the reverse) fails here, on every
/// real-world shape the fixtures hold.
#[test]
fn every_job_of_every_fixture_is_filed_under_its_link() {
    let mut failures = Vec::new();
    for eml in fixtures() {
        let (_d, repo) = repo();
        let raw = std::fs::read(&eml).unwrap();
        let mail = InboundMail::from_rfc822("fixture", &raw).unwrap();
        let (row, jobs) = ingest(&repo, &mail);
        let view = build(&row, &jobs, &ParserConfig::default());
        let name = eml.file_name().unwrap().to_string_lossy().to_string();

        let placed: HashSet<i64> = view.jobs.iter().map(|p| p.job_id).collect();
        for j in &jobs {
            if !placed.contains(&j.id) {
                failures.push(format!("{name}: job {} ({}) is not listed", j.id, j.url));
            }
        }
        for p in &view.jobs {
            let job = jobs.iter().find(|j| j.id == p.job_id).unwrap();
            let want = if job.url.starts_with("attachment://") { "a" } else { "l" };
            match &p.link {
                Some(l) if l.starts_with(want) => {}
                other => failures.push(format!("{name}: {} has no place in the text ({other:?})", job.url)),
            }
        }
        for l in &view.links {
            if l.role == LineRole::Read && l.jobs.is_empty() && l.skip.is_none() {
                failures.push(format!("{name}: {} has neither jobs nor a reason", l.url));
            }
        }
        // Nothing of the text is lost on the way.
        let body = mail.readable_body().replace(['\u{200B}', '\u{FEFF}', '\u{00AD}'], "");
        let shown = shown(&view);
        for word in body.split_whitespace().filter(|w| !w.starts_with("[cid:")) {
            if !shown.contains(word) {
                failures.push(format!("{name}: «{word}» is missing from the shown text"));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_reply_shows_its_quoted_history_marked_unread() {
    let body = "Δείτε εδώ:
https://www.youtube.com/watch?v=reply000001

Στις Τρί 6 Οκτ 2026 στις 19:02, ο/η Master Desk έγραψε:
> στείλε τα θέματα
> https://www.youtube.com/watch?v=old00000001";
    let lines = view_lines(body, "RE: θέματα");
    let roles: Vec<(LineRole, &str)> = lines.iter().map(|l| (l.role, l.text.as_str())).collect();
    assert_eq!(roles[0], (LineRole::Read, "Δείτε εδώ:"));
    assert_eq!(roles[1].0, LineRole::Read);
    assert!(roles.iter().any(|(r, t)| *r == LineRole::Quoted && t.contains("έγραψε")), "{roles:?}");
    assert!(roles.iter().any(|(r, t)| *r == LineRole::Quoted && t.contains("old00000001")), "{roles:?}");

    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("RE: θέματα", body));
    let view = build(&row, &jobs, &ParserConfig::default());
    assert_eq!(jobs.len(), 1, "only the new link is queued");
    let old = view.links.iter().find(|l| l.url.contains("old00000001")).unwrap();
    let skip = old.skip.as_ref().unwrap();
    assert!(skip.reason.contains("παλιότερο μήνυμα"), "{skip:?}");
    assert!(skip.can_queue, "MCR may still want a link from the history");
}

#[test]
fn a_signature_is_shown_marked_unread_and_its_links_are_not_offered() {
    let body = "1. ΣΕΙΣΜΟΣ
https://www.youtube.com/watch?v=sign0000001
--
Ελένη Γεωργίου
https://www.facebook.com/example.newsroom";
    let lines = view_lines(body, "ΘΕΜΑΤΑ");
    assert!(lines.iter().any(|l| l.role == LineRole::Signature && l.text.contains("facebook")));
    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("ΘΕΜΑΤΑ", body));
    let view = build(&row, &jobs, &ParserConfig::default());
    let sig = view.links.iter().find(|l| l.url.contains("facebook")).unwrap();
    let skip = sig.skip.as_ref().unwrap();
    assert!(skip.reason.contains("υπογραφή") && !skip.can_queue, "{skip:?}");
    assert_eq!(view.text.as_ref().unwrap().last().unwrap().role, LineRole::Signature);
}

#[test]
fn a_forward_reads_the_forwarded_mail_and_marks_its_headers() {
    let body = "Για το δελτίο.\n\nΑπό: Άννα Παπαδάκη <a.papadaki@example.gr>\nΣτάλθηκε: Τετάρτη, 7 Οκτωβρίου 2026 11:55\nΠρος: Master Desk <master@example.gr>\nΘέμα: VIRAL\n\nhttps://www.instagram.com/reel/DdX8kq2sAbC/";
    let lines = view_lines(body, "ΠΡ: VIRAL");
    let header = lines.iter().find(|l| l.text.starts_with("Στάλθηκε")).unwrap();
    assert_eq!(header.role, LineRole::Forwarded);
    let link = lines.iter().find(|l| l.text.contains("instagram")).unwrap();
    assert_eq!(link.role, LineRole::Read);
}

#[test]
fn a_link_a_client_wrapped_is_one_link_and_one_job() {
    let body = "1. ΣΕΙΣΜΟΣ\nhttps://www.portal-news.gr/ellada/2026/10/07/seismos-4-8-richter-irakleio-kentro-tis-polis-zimies-\nkatastimata-video\n";
    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("ΘΕΜΑΤΑ", body));
    assert_eq!(jobs.len(), 1);
    let view = build(&row, &jobs, &ParserConfig::default());
    assert_eq!(view.links.len(), 1, "{:?}", view.links);
    assert!(view.links[0].url.ends_with("zimies-katastimata-video"));
    assert_eq!(view.links[0].jobs, vec![jobs[0].id]);
}

#[test]
fn a_safe_links_wrapper_is_shown_as_written_and_filed_with_its_target() {
    let body = "Δείτε: https://eur02.safelinks.protection.outlook.com/?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Dsafe0000001&data=05%7C01&reserved=0.";
    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("ΘΕΜΑ", body));
    assert_eq!(jobs[0].url, "https://www.youtube.com/watch?v=safe0000001");
    let view = build(&row, &jobs, &ParserConfig::default());
    let link = &view.links[0];
    assert_eq!(link.jobs, vec![jobs[0].id]);
    // The chip covers the wrapper as written, without the full stop after it.
    let chip = view.text.as_ref().unwrap()[0].lines[0]
        .iter()
        .find_map(|s| match s {
            Segment::Link { t, .. } => Some(t.clone()),
            _ => None,
        })
        .unwrap();
    assert!(chip.starts_with("https://eur02.safelinks"), "{chip}");
    assert!(chip.ends_with("reserved=0"), "{chip}");
}

#[test]
fn skipped_links_say_why_and_photos_cannot_be_queued() {
    let body = "1. ΕΓΚΑΙΝΙΑ\nhttps://www.youtube.com/watch?v=video000001\nΦωτογραφία: https://www.example.org/press/photo1.jpg\nΚείμενο: https://www.example.org/press/deltio.pdf\nΔείτε και https://www.lasithi-news.example/";
    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("ΘΕΜΑΤΑ", body));
    let view = build(&row, &jobs, &ParserConfig::default());
    let reason = |part: &str| view.links.iter().find(|l| l.url.contains(part)).unwrap().skip.clone().unwrap();
    let photo = reason("photo1.jpg");
    assert!(photo.reason.contains("Φωτογραφία") && !photo.can_queue, "{photo:?}");
    let doc = reason("deltio.pdf");
    assert!(doc.reason.contains("Έγγραφο") && !doc.can_queue, "{doc:?}");
    let front = reason("lasithi-news");
    assert!(front.reason.contains("αρχική σελίδα") && front.can_queue, "{front:?}");
    assert!(view.links.iter().find(|l| l.url.contains("video000001")).unwrap().skip.is_none());
}

#[test]
fn videos_found_in_an_article_are_filed_under_its_link() {
    let body = "1. ΗΘΟΠΟΙΟΣ\nhttps://www.iefimerida.gr/zoi/ithopoios-podilato";
    let (_d, repo) = repo();
    let (row, mut jobs) = ingest(&repo, &mail("ΘΕΜΑΤΑ", body));
    let parent = jobs[0].id;
    let mut sibling = NewJob::new("https://x.com/i/status/2101234567890", "1B_GEORGIOU_ITHOPOIOS", "GEORGIOU");
    sibling.index_str = "1B".into();
    sibling.email_message_id = Some(row.internet_message_id.clone());
    sibling.parent_job_id = Some(parent);
    let child = repo.enqueue(&sibling, DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    jobs = repo.jobs_for_mail(&row.internet_message_id, &[]).unwrap();

    let view = build(&row, &jobs, &ParserConfig::default());
    assert_eq!(view.links[0].jobs, vec![parent, child], "both are the article link's");
    let places: Vec<(i64, Option<i64>)> = view.jobs.iter().map(|p| (p.job_id, p.parent)).collect();
    assert_eq!(places, vec![(parent, None), (child, Some(parent))]);
    assert_eq!(view.jobs[1].link.as_deref(), Some("l1"));
}

#[test]
fn a_discarded_job_leaves_its_link_queueable_again() {
    let body = "1. ΑΓΩΝΑΣ\nhttps://www.youtube.com/watch?v=gone0000001";
    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("ΘΕΜΑΤΑ", body));
    repo.delete_job(jobs[0].id).unwrap();
    let view = build(&row, &[], &ParserConfig::default());
    let skip = view.links[0].skip.clone().unwrap();
    assert!(skip.reason.contains("Αφαιρέθηκε") && skip.can_queue, "{skip:?}");
    assert!(view.jobs.is_empty());
}

#[test]
fn jobs_recorded_with_a_glued_annotation_stay_filed_under_their_links() {
    // P3.12, from the desk on 2026-10-07: mail handled before P3.9 recorded
    // "…-BINTEO" / "…/-ΒΙΝΤΕΟ" as the links. The text now ends each link
    // before the word, so every job, delivered ones included, was shown as
    // a link nobody downloaded ("Δεν επιλέχθηκε…").
    let body = "https://www.youtube.com/watch?v=fixture3001-BINTEO\nhttps://www.news247.gr/kosmos/fixture3002-arthro/-ΒΙΝΤΕΟ";
    let (_d, repo) = repo();
    let key = "<old@example.gr>".to_string();
    let mut records = Vec::new();
    for (i, url) in body.lines().enumerate() {
        let index = format!("1{}", ['A', 'B'][i]);
        let slug = format!("{index}_GEORGIOU_PLANA");
        let mut new = NewJob::new(url, slug.clone(), "GEORGIOU");
        new.index_str = index.clone();
        new.email_message_id = Some(key.clone());
        let result = repo.enqueue(&new, DEFAULT_DEDUP_WINDOW_HOURS).unwrap();
        records.push(QueuedFromMail {
            index_str: index,
            slug,
            url: url.to_string(),
            status: "PENDING".into(),
            result,
            attachment_id: None,
        });
    }
    let row = ProcessedMail {
        internet_message_id: key.clone(),
        outcome: "JOBS".into(),
        subject: Some("ΥΛΙΚΟ ΓΙΑ ΠΛΑΝΑ".into()),
        jobs_json: serde_json::to_string(&records).unwrap(),
        body_text: Some(body.into()),
        ..Default::default()
    };
    let jobs = repo.jobs_for_mail(&key, &[]).unwrap();
    let view = build(&row, &jobs, &ParserConfig::default());

    assert_eq!(view.links.len(), 2);
    assert_eq!(view.links[0].url, "https://www.youtube.com/watch?v=fixture3001");
    assert_eq!(view.links[1].url, "https://www.news247.gr/kosmos/fixture3002-arthro/");
    for (l, j) in view.links.iter().zip(&jobs) {
        assert_eq!(l.jobs, vec![j.id], "{} lost its job", l.url);
        assert!(l.skip.is_none(), "{} shown as not downloaded: {:?}", l.url, l.skip);
    }
}

#[test]
fn a_link_already_queued_by_another_mail_shows_that_job() {
    let (_d, repo) = repo();
    let first = repo
        .enqueue(&NewJob::new("https://www.youtube.com/watch?v=shared00001", "1_MCR_X", "MCR"), DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    let (row, _) = ingest(&repo, &mail("ΘΕΜΑΤΑ", "1. ΑΓΩΝΑΣ\nhttps://www.youtube.com/watch?v=shared00001"));
    let queued: Vec<QueuedFromMail> = serde_json::from_str(&row.jobs_json).unwrap();
    assert_eq!(queued[0].result, Enqueued::DuplicateActive { existing_id: first });
    let jobs = repo.jobs_for_mail(&row.internet_message_id, &[first]).unwrap();
    let view = build(&row, &jobs, &ParserConfig::default());
    assert_eq!(view.links[0].jobs, vec![first]);
    assert!(view.jobs[0].shared, "it is another entry's job");
}

#[test]
fn a_link_mcr_queues_from_the_text_is_named_after_its_neighbours() {
    let body = "1. ΣΕΙΣΜΟΣ\nhttps://www.youtube.com/watch?v=near0000001\nΔείτε και https://www.lasithi-news.example/\n\n2. ΚΑΙΡΟΣ\nhttps://www.youtube.com/watch?v=near0000002";
    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("ΘΕΜΑΤΑ", body));
    let view = build(&row, &jobs, &ParserConfig::default());
    assert_eq!(view.next_index, "3");
    let skipped = view.links.iter().find(|l| l.url.contains("lasithi")).unwrap();
    let (journalist, keyword, index) = mail_view::naming_for_link(&row, &view, &jobs, &skipped.id).unwrap();
    assert_eq!((journalist.as_str(), keyword.as_str(), index.as_str()), ("GEORGIOU", "SEISMOS", "3"));
}

#[test]
fn notes_explain_in_greek_what_the_parser_did() {
    let (_d, repo) = repo();
    let mut m = mail("ΕΚΤΑΚΤΟ: ΘΕΜΑΤΑ", "1. ΑΡΘΡΟ\nhttps://www.iefimerida.gr/zoi/arthro-test\nΓΙΑ ΠΛΑΝΑ: 2 ΠΡΩΤΑ ΒΙΝΤΕΟ");
    m.from_address = "unknown@example.org".into();
    let (row, jobs) = ingest(&repo, &m);
    let view = build(&row, &jobs, &ParserConfig::default());
    let texts: Vec<&str> = view.notes.iter().map(|n| n.text.as_str()).collect();
    assert!(texts.iter().any(|t| t.starts_with("Επείγον")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("«2 ΠΡΩΤΑ ΒΙΝΤΕΟ»")), "{texts:?}");
    let unresolved = view.notes.iter().find(|n| n.text.contains("Δεν βρέθηκε δημοσιογράφος")).unwrap();
    assert_eq!(unresolved.level, NoteLevel::Warn);
}

/// The text goes out as data. A mail whose body is markup is shown as that
/// markup, character for character, in text runs.
#[test]
fn a_hostile_body_comes_out_as_plain_text() {
    let body = "<img src=x onerror=alert(1)> https://www.youtube.com/watch?v=xss00000001\"><script>alert(2)</script>";
    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("ΘΕΜΑ", body));
    let view = build(&row, &jobs, &ParserConfig::default());
    assert_eq!(shown(&view), body);
    let link = view.text.as_ref().unwrap()[0].lines[0]
        .iter()
        .find_map(|s| match s {
            Segment::Link { t, .. } => Some(t.clone()),
            _ => None,
        })
        .unwrap();
    assert!(!link.contains('<') && !link.contains('"'), "{link}");
}

#[test]
fn a_mail_without_kept_text_still_lists_its_jobs() {
    let (_d, repo) = repo();
    let (mut row, jobs) = ingest(&repo, &mail("ΘΕΜΑΤΑ", "1. ΑΓΩΝΑΣ\nhttps://www.youtube.com/watch?v=notext00001"));
    row.body_text = None;
    let view = build(&row, &jobs, &ParserConfig::default());
    assert!(view.text.is_none());
    assert_eq!(view.jobs.len(), 1);
    assert_eq!(view.jobs[0].link, None);
}

#[test]
fn the_preview_is_the_text_the_parser_read_with_short_links() {
    let body = "Καλησπέρα,\n1. ΣΕΙΣΜΟΣ\nhttps://www.youtube.com/watch?v=prev0000001\n--\nΕλένη Γεωργίου\nΤμήμα Ειδήσεων";
    let p = mail_view::preview(body, "ΘΕΜΑΤΑ", 200);
    assert_eq!(p, "Καλησπέρα, 1. ΣΕΙΣΜΟΣ youtube.com/watch?v=prev0000001");
    let short = mail_view::preview(&"λέξη ".repeat(100), "", 20);
    assert!(short.chars().count() <= 21 && short.ends_with('…'), "{short}");
}

#[test]
fn indices_sort_as_a_journalist_numbers_them() {
    let mut v = vec!["10", "2", "1B", "1", "1A", "1AA"];
    v.sort_by(|a, b| mail_view::index_order(a, b));
    assert_eq!(v, vec!["1", "1A", "1B", "1AA", "2", "10"]);
}

#[test]
fn jobs_keep_their_statuses_in_the_view_input() {
    // `build` reads statuses only through the jobs it is given; a job the
    // desk shows as delivered is a delivered row.
    let (_d, repo) = repo();
    let (row, jobs) = ingest(&repo, &mail("ΘΕΜΑΤΑ", "1. Α\nhttps://www.youtube.com/watch?v=status00001"));
    let leased = repo.lease_job("host:1:0", 60).unwrap().unwrap();
    repo.finish(leased.id, "host:1:0", JobStatus::Completed, None, None, Some("W:/x.mxf")).unwrap();
    let jobs_now = repo.jobs_for_mail(&row.internet_message_id, &[]).unwrap();
    assert_eq!(jobs_now[0].status, JobStatus::Completed);
    let view = build(&row, &jobs_now, &ParserConfig::default());
    assert_eq!(view.links[0].jobs, vec![jobs[0].id]);
}
