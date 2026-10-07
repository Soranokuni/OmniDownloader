//! What the MCR mail view shows about one mail (plan P7.6, P7.7).
//!
//! The watcher stores a [`MailParseSummary`] with each handled mail, so the
//! desk can say who the mail was filed under and why, which sections it had
//! and what the parser warned about, without parsing the mail again (the
//! roster and the parser may have changed since).
//!
//! [`build`] turns a stored mail and its jobs into what the desk draws: the
//! text as the parser saw it (what it read, what it skipped as quoted history
//! or signature), every link in it with the jobs it became or why it became
//! none, and the jobs in the order the desk lists them. It is pure: the same
//! row and jobs always give the same view, and the tests pin it down.
//!
//! Nothing here is markup. The text goes out as plain strings and the panel
//! puts it in text nodes; a mail whose body is `<img onerror=…>` is shown as
//! exactly that.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use omni_core::models::{Job, ProcessedMail};
use omni_core::urlnorm;

use crate::groups::ResolvedGroup;
use crate::mail::AttachmentMeta;
use crate::parser::{self, warnings, LineRole, Outcome, ParsedEmail, ParserConfig, Resolution, Tier, Warning};
use crate::watcher::QueuedFromMail;

/// What the parser decided about one mail, as stored in
/// `processed_mail.parse_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailParseSummary {
    pub journalist: String,
    pub how: Resolution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<ResolvedGroup>,
    #[serde(default)]
    pub urgent: bool,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<SectionSummary>,
}

/// One numbered section of the mail: the number the journalist wrote, the
/// title and the keyword its files were named with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectionSummary {
    pub index_str: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyword: Option<String>,
    /// "2 ΠΡΩΤΑ ΒΙΝΤΕΟ" in this section (plan P4.33).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_videos: Option<u32>,
}

impl MailParseSummary {
    pub fn of(parsed: &ParsedEmail) -> Self {
        Self {
            journalist: parsed.journalist.surname.clone(),
            how: parsed.journalist.how,
            group: parsed.group.clone(),
            urgent: parsed.urgent,
            outcome: parsed.outcome,
            warnings: parsed.warnings.clone(),
            ignored_urls: parsed.ignored_urls.clone(),
            sections: parsed
                .sections
                .iter()
                .map(|s| SectionSummary {
                    index_str: s.index_str.clone(),
                    title: s.title.clone(),
                    keyword: s.keyword.clone(),
                    max_videos: s.jobs.iter().find_map(|j| j.max_videos),
                })
                .collect(),
        }
    }

    /// The summary stored with `mail`, if any (none before plan P7.6).
    pub fn stored(mail: &ProcessedMail) -> Option<Self> {
        mail.parse_json.as_deref().and_then(|j| serde_json::from_str(j).ok())
    }
}

/// The stored text is capped: a mail is read by a person on the desk, and a
/// newsletter that renders to megabytes of text is not one they will read.
pub const MAX_STORED_TEXT: usize = 256 * 1024;

/// `text` cut to at most [`MAX_STORED_TEXT`] bytes, on a character boundary.
pub fn stored_text(text: &str) -> String {
    if text.len() <= MAX_STORED_TEXT {
        return text.to_string();
    }
    let mut end = MAX_STORED_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…", &text[..end])
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MailView {
    /// The text in runs of lines with the same role. `None` when it was not
    /// kept: mail handled before plan P7.6, or past the retention period.
    pub text: Option<Vec<TextBlock>>,
    /// Every link in the text, in order; `Segment::Link::l` points here.
    pub links: Vec<LinkView>,
    pub attachments: Vec<AttachmentView>,
    /// The mail's jobs in the order the desk lists them: as the parser queued
    /// them, each followed by the videos found in its article.
    pub jobs: Vec<JobPlace>,
    /// What the parser noticed, for a person, in Greek.
    pub notes: Vec<Note>,
    /// The number a link queued from this mail by MCR gets.
    pub next_index: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextBlock {
    pub role: LineRole,
    pub lines: Vec<Vec<Segment>>,
}

/// A run of a line: plain text, or a link (`l` is its [`LinkView::id`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Segment {
    Link { l: String, t: String },
    Text { t: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkView {
    pub id: String,
    /// The address as the parser reads it: scheme added, redirects unwrapped.
    pub url: String,
    pub role: LineRole,
    /// The jobs filed under this link: the one it became and the videos
    /// found in its article. Empty when it became none.
    pub jobs: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<Skip>,
}

/// Why a link became no job, and whether MCR may queue it from the mail.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skip {
    pub reason: String,
    pub can_queue: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttachmentView {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub content_type: String,
    pub jobs: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobPlace {
    pub job_id: i64,
    /// The link (`l…`) or attachment (`a…`) it came from, when it is there.
    pub link: Option<String>,
    /// The job whose article it was found in.
    pub parent: Option<i64>,
    /// The link was already queued (by another mail, or by hand): this is
    /// that job, not one of this mail's own.
    pub shared: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteLevel {
    Info,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    pub level: NoteLevel,
    pub text: String,
}

/// How the journalist was found, as the desk says it.
pub fn how_el(how: Resolution) -> &'static str {
    match how {
        Resolution::BodyOverride => "από το κείμενο («ΣΤΟ ΟΝΟΜΑ …»)",
        Resolution::BodyOverrideSender => "από το κείμενο («ΣΤΟ ΟΝΟΜΑ ΜΟΥ»)",
        Resolution::Subject => "από το θέμα του email",
        Resolution::Sender => "από τη διεύθυνση του αποστολέα",
        Resolution::Forwarded => "από το προωθημένο μήνυμα",
        Resolution::Unresolved => "δεν βρέθηκε· τα αρχεία πήραν το όνομα MCR",
        Resolution::LlmAssist => "πρόταση του LLM, επιβεβαιωμένη από τη λίστα",
    }
}

/// The view of `mail`. `jobs` holds the mail's own jobs (those with its
/// Message-ID, article siblings included) and the jobs its duplicate links
/// pointed at; a job missing from it was discarded.
pub fn build(mail: &ProcessedMail, jobs: &[Job], cfg: &ParserConfig) -> MailView {
    let subject = mail.subject.as_deref().unwrap_or("");
    let summary = MailParseSummary::stored(mail);
    let queued: Vec<QueuedFromMail> = serde_json::from_str(&mail.jobs_json).unwrap_or_default();
    let by_id: HashMap<i64, &Job> = jobs.iter().map(|j| (j.id, j)).collect();

    // Videos found in an article, by the job of the article.
    let mut children: BTreeMap<i64, Vec<&Job>> = BTreeMap::new();
    for j in jobs {
        if let Some(p) = j.parent_job_id.filter(|p| by_id.contains_key(p) && *p != j.id) {
            children.entry(p).or_default().push(j);
        }
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| index_order(&a.index_str, &b.index_str).then(a.id.cmp(&b.id)));
    }
    let with_children = |id: i64| -> Vec<i64> {
        let mut out = vec![id];
        if let Some(list) = children.get(&id) {
            out.extend(list.iter().map(|c| c.id));
        }
        out
    };

    // What the watcher (and MCR, from the mail) queued for each link.
    let mut recorded: Vec<(String, i64)> = Vec::new();
    let mut by_attachment: HashMap<String, Vec<i64>> = HashMap::new();
    for q in &queued {
        let id = q.result.job_id();
        match &q.attachment_id {
            Some(a) => by_attachment.entry(a.clone()).or_default().push(id),
            None => recorded.push((urlnorm::normalize(&q.url), id)),
        }
    }
    let filed_under = |key: &str| -> (Vec<i64>, bool) {
        let mut out: Vec<i64> = Vec::new();
        let mut gone = false;
        for (k, id) in &recorded {
            if k != key {
                continue;
            }
            if by_id.contains_key(id) {
                for x in with_children(*id) {
                    if !out.contains(&x) {
                        out.push(x);
                    }
                }
            } else {
                gone = true;
            }
        }
        let removed = gone && out.is_empty();
        (out, removed)
    };

    // The text.
    let mut links: Vec<LinkView> = Vec::new();
    let text = mail.body_text.as_deref().map(|body| {
        let mut blocks: Vec<TextBlock> = Vec::new();
        for line in parser::view_lines(body, subject) {
            let mut segs = Vec::new();
            let mut at = 0;
            for (range, url) in &line.links {
                if range.start > at {
                    segs.push(Segment::Text { t: line.text[at..range.start].to_string() });
                }
                let id = format!("l{}", links.len() + 1);
                let (ids, removed) = filed_under(&urlnorm::normalize(url));
                let skip = ids.is_empty().then(|| skip_for(url, line.role, removed, cfg));
                links.push(LinkView { id: id.clone(), url: url.clone(), role: line.role, jobs: ids, skip });
                segs.push(Segment::Link { l: id, t: line.text[range.clone()].to_string() });
                at = range.end;
            }
            if at < line.text.len() || segs.is_empty() {
                segs.push(Segment::Text { t: line.text[at..].to_string() });
            }
            match blocks.last_mut() {
                Some(b) if b.role == line.role => b.lines.push(segs),
                _ => blocks.push(TextBlock { role: line.role, lines: vec![segs] }),
            }
        }
        blocks
    });

    // Attachments.
    let metas: Vec<AttachmentMeta> =
        mail.attachments_json.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
    let attachments: Vec<AttachmentView> = metas
        .iter()
        .enumerate()
        .map(|(i, a)| AttachmentView {
            id: format!("a{}", i + 1),
            name: a.name.clone(),
            size: a.size,
            content_type: a.content_type.clone(),
            jobs: by_attachment
                .get(&a.id)
                .map(|ids| ids.iter().copied().filter(|id| by_id.contains_key(id)).collect())
                .unwrap_or_default(),
        })
        .collect();

    // Where each job sits: the first link (or attachment) filed with it.
    let mut place_of: HashMap<i64, String> = HashMap::new();
    for l in &links {
        for id in &l.jobs {
            place_of.entry(*id).or_insert_with(|| l.id.clone());
        }
    }
    for a in &attachments {
        for id in &a.jobs {
            place_of.entry(*id).or_insert_with(|| a.id.clone());
        }
    }

    // The desk's order: as queued, each followed by its article's videos;
    // then anything of the mail's own queued since (by MCR, from the mail).
    let own = |j: &Job| j.email_message_id.as_deref() == Some(mail.internet_message_id.as_str());
    let mut order: Vec<JobPlace> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let push = |order: &mut Vec<JobPlace>, seen: &mut HashSet<i64>, j: &Job, parent: Option<i64>| {
        if !seen.insert(j.id) {
            return;
        }
        order.push(JobPlace {
            job_id: j.id,
            link: place_of.get(&j.id).cloned(),
            parent,
            shared: !own(j),
        });
        if let Some(list) = children.get(&j.id) {
            for c in list {
                if seen.insert(c.id) {
                    order.push(JobPlace { job_id: c.id, link: place_of.get(&c.id).cloned(), parent: Some(j.id), shared: false });
                }
            }
        }
    };
    for q in &queued {
        if let Some(j) = by_id.get(&q.result.job_id()) {
            push(&mut order, &mut seen, j, None);
        }
    }
    let mut rest: Vec<&Job> = jobs.iter().filter(|j| own(j) && !seen.contains(&j.id)).collect();
    rest.sort_by_key(|j| j.id);
    for j in rest {
        if seen.contains(&j.id) {
            continue;
        }
        let parent = j.parent_job_id.filter(|p| by_id.contains_key(p));
        match parent.and_then(|p| order.iter().rposition(|x| x.job_id == p || x.parent == Some(p))) {
            Some(at) if seen.insert(j.id) => {
                order.insert(at + 1, JobPlace { job_id: j.id, link: place_of.get(&j.id).cloned(), parent, shared: false });
            }
            _ => push(&mut order, &mut seen, j, parent),
        }
    }

    MailView {
        notes: notes(mail, summary.as_ref()),
        next_index: next_index(&queued, jobs.iter().filter(|j| own(j))),
        text,
        links,
        attachments,
        jobs: order,
    }
}

/// `1` < `1A` < `1B` < `2` < `10`: the number first, then the letters.
pub fn index_order(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        (digits.parse::<u32>().unwrap_or(u32::MAX), s[digits.len()..].len(), s[digits.len()..].to_string())
    };
    split(a).cmp(&split(b))
}

/// One more than the highest section number this mail has used.
fn next_index<'a>(queued: &[QueuedFromMail], own: impl Iterator<Item = &'a Job>) -> String {
    let lead = |s: &str| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().ok();
    let max = queued
        .iter()
        .filter_map(|q| lead(&q.index_str))
        .chain(own.filter_map(|j| lead(&j.index_str)))
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}

/// Why a link in the text became no job.
fn skip_for(url: &str, role: LineRole, removed: bool, cfg: &ParserConfig) -> Skip {
    let skip = |reason: &str, can_queue: bool| Skip { reason: reason.to_string(), can_queue };
    if removed {
        return skip("Αφαιρέθηκε από το MCR.", true);
    }
    if crate::decontaminate::is_mail_client_link(url) {
        return skip("Σύνδεσμος του προγράμματος email, όχι του αποστολέα.", false);
    }
    let tier = parser::classify(url, cfg);
    if tier == Tier::Image {
        return skip("Φωτογραφία: κατεβαίνουν μόνο βίντεο.", false);
    }
    if parser::is_document_link(url) {
        return skip("Έγγραφο, όχι σελίδα με βίντεο.", false);
    }
    match role {
        LineRole::Signature => skip("Στην υπογραφή, που το σύστημα δεν διαβάζει.", false),
        LineRole::Quoted => skip("Σε παλιότερο μήνυμα από κάτω, που το σύστημα δεν διαβάζει.", true),
        LineRole::Forwarded => skip("Στις κεφαλίδες του προωθημένου μηνύματος.", true),
        LineRole::Read if tier == Tier::Other && !parser::could_be_a_story(url) => {
            skip("Δεν μοιάζει με σελίδα είδησης: αρχική σελίδα, προφίλ, χάρτης ή φόρμα.", true)
        }
        LineRole::Read => skip("Δεν επιλέχθηκε, π.χ. επειδή το «ΓΙΑ ΠΛΑΝΑ» έδειξε άλλους συνδέσμους.", true),
    }
}

/// What the parser noticed, for a person.
fn notes(mail: &ProcessedMail, summary: Option<&MailParseSummary>) -> Vec<Note> {
    let info = |text: String| Note { level: NoteLevel::Info, text };
    let warn = |text: String| Note { level: NoteLevel::Warn, text };
    let mut out = Vec::new();
    if mail.outcome == "FAILED" {
        out.push(warn(
            "Το σύστημα δεν μπόρεσε να διαβάσει αυτό το email μετά από επανειλημμένες προσπάθειες. \
             Δεν μπήκε κανένα βίντεο στην ουρά."
                .into(),
        ));
        return out;
    }
    let Some(s) = summary else {
        if mail.body_text.is_none() {
            out.push(info("Το κείμενο αυτού του email δεν κρατήθηκε (παλιότερο email).".into()));
        }
        return out;
    };
    if s.urgent {
        out.push(info("Επείγον: τα βίντεο αυτού του email μπήκαν μπροστά στην ουρά.".into()));
    }
    for sec in &s.sections {
        if let Some(n) = sec.max_videos {
            out.push(info(if n == 1 {
                format!("Θέμα {}: «ΜΟΝΟ ΤΟ ΠΡΩΤΟ ΒΙΝΤΕΟ». Από το άρθρο κατεβαίνει μόνο το πρώτο· τα υπόλοιπα προτείνονται.", sec.index_str)
            } else {
                format!("Θέμα {}: «{n} ΠΡΩΤΑ ΒΙΝΤΕΟ». Από το άρθρο κατεβαίνουν μόνο τα {n} πρώτα· τα υπόλοιπα προτείνονται.", sec.index_str)
            }));
        }
    }
    for w in &s.warnings {
        let detail = w.detail.clone().unwrap_or_default();
        let note = match w.code.as_str() {
            warnings::JOURNALIST_UNRESOLVED => {
                warn("Δεν βρέθηκε δημοσιογράφος: τα αρχεία πήραν το όνομα MCR.".into())
            }
            warnings::JOURNALIST_AMBIGUOUS => {
                warn("Το όνομα ταιριάζει σε περισσότερους από έναν δημοσιογράφους· ελέγξτε ποιος είναι.".into())
            }
            warnings::JOURNALIST_OVERRIDE_UNKNOWN => warn(format!(
                "Το «ΣΤΟ ΟΝΟΜΑ» αναφέρει «{detail}», που δεν είναι στη λίστα δημοσιογράφων."
            )),
            warnings::JOURNALIST_SUGGESTED => warn(format!(
                "Το email απευθύνεται σε «{detail}», που δεν είναι στη λίστα δημοσιογράφων. Αν είναι συνάδελφος, προσθέστε τον."
            )),
            warnings::PREAMBLE_URLS => info("Υπήρχαν σύνδεσμοι πριν από το πρώτο θέμα· μπήκαν στο θέμα 1.".into()),
            warnings::SECTION_WITHOUT_LINKS => warn(format!("Το θέμα {detail} δεν είχε σύνδεσμο ή βίντεο.")),
            warnings::DUPLICATE_SECTION_NUMBER => warn("Ο ίδιος αριθμός θέματος γράφτηκε δύο φορές.".into()),
            warnings::MARKER_WITHOUT_URL => warn(format!("Στο θέμα {detail} υπήρχε «ΓΙΑ ΠΛΑΝΑ:» χωρίς σύνδεσμο.")),
            warnings::ATTACHMENT_TOO_LARGE => warn(format!(
                "Το συνημμένο «{detail}» είναι πολύ μεγάλο για αυτόματη λήψη: κατεβάστε το από το email."
            )),
            warnings::GROUP_AMBIGUOUS => info("Η ομάδα δεν ήταν σαφής από το email.".into()),
            warnings::LLM_ASSIST_APPLIED => info("Το LLM βοήθησε στο όνομα ή στη λέξη-κλειδί.".into()),
            _ => continue,
        };
        out.push(note);
    }
    match s.outcome {
        Outcome::PhotosOnly => out.push(info("Μόνο φωτογραφίες: δεν υπάρχει βίντεο για λήψη.".into())),
        Outcome::NoLinks => out.push(info("Δεν βρέθηκε σύνδεσμος ή συνημμένο βίντεο σε αυτό το email.".into())),
        Outcome::Jobs => {}
    }
    out
}

/// Journalist, keyword and number for a link MCR queues from the mail's
/// text: the mail's journalist; the keyword of the nearest link before it
/// that became a job (the same story, most likely), else after it, else the
/// mail's first section, else the link's own words, else `ASSET`.
pub fn naming_for_link(mail: &ProcessedMail, view: &MailView, jobs: &[Job], link_id: &str) -> Option<(String, String, String)> {
    let at = view.links.iter().position(|l| l.id == link_id)?;
    let summary = MailParseSummary::stored(mail);
    let by_id: HashMap<i64, &Job> = jobs.iter().map(|j| (j.id, j)).collect();
    let keyword_of = |l: &LinkView| l.jobs.first().and_then(|id| by_id.get(id)).map(|j| j.keyword.clone());
    let keyword = view.links[..at]
        .iter()
        .rev()
        .find_map(keyword_of)
        .or_else(|| view.links[at + 1..].iter().find_map(keyword_of))
        .or_else(|| summary.as_ref().and_then(|s| s.sections.first()).and_then(|s| s.keyword.clone()))
        .or_else(|| parser::keyword_from_url(&view.links[at].url))
        .unwrap_or_else(|| "ASSET".into());
    let journalist = summary
        .as_ref()
        .map(|s| s.journalist.clone())
        .or_else(|| jobs.iter().find(|j| !j.journalist.is_empty()).map(|j| j.journalist.clone()))
        .unwrap_or_else(|| "MCR".into());
    Some((journalist, keyword, view.next_index.clone()))
}

/// The first words of what the parser read, links shortened, for the list.
pub fn preview(body: &str, subject: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for line in parser::view_lines(body, subject) {
        if line.role != LineRole::Read {
            continue;
        }
        let mut text = line.text.clone();
        for (r, _) in line.links.iter().rev() {
            let raw = &line.text[r.clone()];
            let lower = raw.to_ascii_lowercase();
            let cut = ["https://www.", "http://www.", "https://", "http://", "www."]
                .iter()
                .find(|p| lower.starts_with(*p))
                .map(|p| p.len())
                .unwrap_or(0);
            text.replace_range(r.clone(), &raw[cut..]);
        }
        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if words.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&words);
        if out.chars().count() > max_chars {
            break;
        }
    }
    if out.chars().count() > max_chars {
        let cut: String = out.chars().take(max_chars).collect();
        return format!("{}…", cut.trim_end());
    }
    out
}
