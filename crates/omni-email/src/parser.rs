//! Deterministic email parser (plan P4.3).
//!
//! `parse(&InboundMail, &[Journalist], &ParserConfig) -> ParsedEmail` is a pure
//! function: no network, no database, no LLM. The same email always yields the
//! same jobs, and the golden fixtures in `tests/fixtures/` pin that down.
//!
//! The LLM (plan P4.4) may later *refine* the journalist or a keyword, never
//! add URLs or indices, so everything that decides what goes to air lives here.

use regex::Regex;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;

use omni_core::models::{JobStatus, Journalist};
use omni_core::translit::translit;
use omni_core::urlnorm;

use crate::decontaminate::decontaminate_email_body;
use crate::mail::InboundMail;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

pub use omni_core::config::ParserConfig;

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Video platforms yt-dlp handles directly.
    Tier1,
    /// News portals: the sniffer finds the player in the article.
    Tier2,
    /// File lockers a human fetches (WeTransfer, OneDrive, ...).
    Locker,
    Image,
    Other,
    /// A video file attached to the email itself (plan P4.6).
    Attachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// `ΣΤΟ ΟΝΟΜΑ ΤΗΣ/ΤΟΥ X` in the body.
    BodyOverride,
    /// `ΣΤΟ ΟΝΟΜΑ ΜΟΥ`: explicitly the sender.
    BodyOverrideSender,
    /// `ΘΕΜΑΤΑ X`, `ΕΠΙΚΑΙΡΟΤΗΤΑ X`, `ΓΙΑ (ΜΟΝΤΑΖ) X` in the subject.
    Subject,
    /// The sender address is on the roster.
    Sender,
    /// The mail came from the MCR desk (or an address not on the roster)
    /// passing on a journalist's mail: the forwarded `From:`/`Από:` line in
    /// the body names someone on the roster.
    Forwarded,
    /// Nobody matched; the jobs go to `MCR`. A mail *from* the MCR desk
    /// that names nobody ends here too: MCR passes mail on, it is never the
    /// journalist.
    Unresolved,
    /// Proposed by the LLM (plan P4.4) and confirmed against the roster.
    LlmAssist,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedJournalist {
    pub surname: String,
    pub how: Resolution,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedJob {
    /// `N` for a section's only asset, `NA`, `NB`, … when there are several.
    pub index_str: String,
    pub url: String,
    pub tier: Tier,
    pub keyword: String,
    pub confidence: f64,
    pub status: JobStatus,
    /// Selected by a `ΓΙΑ ΠΛΑΝΑ:` style marker.
    pub marker: bool,
    /// Provider attachment id, for `Tier::Attachment` jobs (plan P4.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    /// The number the journalist wrote, never renumbered.
    pub index_str: String,
    pub title: Option<String>,
    /// Keyword the section's jobs share, derived from the title (or, for an
    /// unnumbered email, the subject). `None` → each job falls back to its
    /// URL slug, then `ASSET`.
    pub keyword: Option<String>,
    pub jobs: Vec<ParsedJob>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Outcome {
    /// At least one job.
    Jobs,
    /// Only photographs: nothing to ingest, the reply says so.
    PhotosOnly,
    /// No usable link or attachment at all.
    NoLinks,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Warning {
    fn new(code: &str, detail: Option<String>) -> Self {
        Self {
            code: code.to_string(),
            detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedEmail {
    pub journalist: ResolvedJournalist,
    /// The group the mail is for (plan P4.18), set by
    /// [`crate::groups::resolve_group`] after parsing; `parse` leaves it
    /// empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<crate::groups::ResolvedGroup>,
    pub outcome: Outcome,
    /// The subject carried an urgent keyword (`ΕΚΤΑΚΤΟ`, …).
    pub urgent: bool,
    pub sections: Vec<Section>,
    /// Links seen but not queued (context links next to a video, images).
    pub ignored_urls: Vec<String>,
    pub warnings: Vec<Warning>,
}

impl ParsedEmail {
    pub fn jobs(&self) -> impl Iterator<Item = &ParsedJob> {
        self.sections.iter().flat_map(|s| s.jobs.iter())
    }

    pub fn has_warning(&self, code: &str) -> bool {
        self.warnings.iter().any(|w| w.code == code)
    }
}

pub mod warnings {
    pub const JOURNALIST_UNRESOLVED: &str = "JOURNALIST_UNRESOLVED";
    pub const JOURNALIST_AMBIGUOUS: &str = "JOURNALIST_AMBIGUOUS";
    pub const JOURNALIST_OVERRIDE_UNKNOWN: &str = "JOURNALIST_OVERRIDE_UNKNOWN";
    pub const PREAMBLE_URLS: &str = "PREAMBLE_URLS";
    pub const SECTION_WITHOUT_LINKS: &str = "SECTION_WITHOUT_LINKS";
    pub const DUPLICATE_SECTION_NUMBER: &str = "DUPLICATE_SECTION_NUMBER";
    pub const MARKER_WITHOUT_URL: &str = "MARKER_WITHOUT_URL";
    pub const ATTACHMENT_TOO_LARGE: &str = "ATTACHMENT_TOO_LARGE";
    /// The LLM was asked and something it said was used (plan P4.4).
    pub const LLM_ASSIST_APPLIED: &str = "LLM_ASSIST_APPLIED";
    /// The LLM read a recipient in the mail who is not on the roster; the
    /// detail is the name as written, for MCR to add (plan P4.26).
    pub const JOURNALIST_SUGGESTED: &str = "JOURNALIST_SUGGESTED";
    /// Several groups were named at one step and membership did not decide.
    pub const GROUP_AMBIGUOUS: &str = "GROUP_AMBIGUOUS";
    /// The LLM was asked and nothing it said was used.
    pub const LLM_ASSIST_SKIPPED: &str = "LLM_ASSIST_SKIPPED";
}

// ---------------------------------------------------------------------------
// Journalist resolution
// ---------------------------------------------------------------------------

/// Letters and digits of the ELOT 743 form: the space all names compare in.
fn latin_key(s: &str) -> String {
    translit(s).chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

fn tokens(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty())
}

enum Lookup {
    One(String),
    /// Several people answer to it (sorted surnames).
    Ambiguous(Vec<String>),
    None,
}

struct Roster<'a> {
    /// (latin key, surname)
    keys: Vec<(String, String)>,
    journalists: &'a [Journalist],
}

impl<'a> Roster<'a> {
    fn new(journalists: &'a [Journalist]) -> Self {
        let mut keys = Vec::new();
        for j in journalists {
            let mut add = |raw: &str| {
                let k = latin_key(raw);
                if !k.is_empty() {
                    keys.push((k, j.surname.clone()));
                }
            };
            add(&j.surname);
            for a in &j.aliases {
                add(a);
            }
            // First names from the full name. Two staff sharing a first name
            // is handled by the ambiguity rule, not by leaving them out.
            for t in tokens(&j.full_name) {
                add(t);
            }
        }
        Self { keys, journalists }
    }

    fn lookup(&self, token: &str) -> Lookup {
        let tok = latin_key(token);
        if tok.len() < 3 {
            return Lookup::None;
        }
        let pick = |set: HashSet<&String>| match set.len() {
            0 => None,
            1 => Some(Lookup::One(set.into_iter().next().unwrap().clone())),
            _ => {
                let mut all: Vec<String> = set.into_iter().cloned().collect();
                all.sort();
                Some(Lookup::Ambiguous(all))
            }
        };
        let exact: HashSet<&String> = self.keys.iter().filter(|(k, _)| *k == tok).map(|(_, s)| s).collect();
        if let Some(l) = pick(exact) {
            return l;
        }
        // Genitive / nominative forms: ΑΝΝΑ ↔ ΑΝΝΑΣ, ΠΑΠΑΔΑΚΗΣ ↔ ΠΑΠΑΔΑΚΗ.
        let prefix: HashSet<&String> = self
            .keys
            .iter()
            .filter(|(k, _)| {
                let (short, long) = if k.len() <= tok.len() { (k, &tok) } else { (&tok, k) };
                short.len() >= 4 && long.len() - short.len() <= 2 && long.starts_with(short.as_str())
            })
            .map(|(_, s)| s)
            .collect();
        pick(prefix).unwrap_or(Lookup::None)
    }

    fn by_sender(&self, address: &str) -> Option<String> {
        let addr = address.trim().to_lowercase();
        if addr.is_empty() {
            return None;
        }
        self.journalists
            .iter()
            .find(|j| j.emails.iter().any(|e| e.trim().to_lowercase() == addr))
            .map(|j| j.surname.clone())
    }

    /// The one person a display name ("Δανάη Λαμπράκη") refers to: every
    /// word that names someone names the same person.
    fn by_display_name(&self, name: &str) -> Option<String> {
        let mut found: HashSet<String> = HashSet::new();
        for t in tokens(name) {
            if let Lookup::One(s) = self.lookup(t) {
                found.insert(s);
            }
        }
        (found.len() == 1).then(|| found.into_iter().next().unwrap())
    }
}

/// `From:`, `To:`, `Cc:` lines of mail forwarded or quoted in the body, as
/// Outlook (English and Greek: Από, Προς, Κοιν.) and Gmail write them.
static RE_QUOTED_PARTIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\*?(from|από|to|προς|cc|κοιν\.?)\s*:\s*(.+)$").unwrap());
static RE_ADDRESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[a-z0-9._%+-]+@[a-z0-9-]+(?:\.[a-z0-9-]+)*\.[a-z]{2,}").unwrap());

/// The roster people in the headers of mail forwarded or quoted in the body.
struct QuotedPeople {
    /// Who sent each forwarded/quoted mail, newest (top of the body) first.
    senders: Vec<String>,
    /// Everyone on any of those From/To/Cc lines, and on the mail's own To/Cc.
    everyone: HashSet<String>,
}

fn quoted_people(mail: &InboundMail, roster: &Roster) -> QuotedPeople {
    let mut senders = Vec::new();
    let mut everyone: HashSet<String> = HashSet::new();
    for addr in mail.to.iter().chain(&mail.cc) {
        if let Some(s) = RE_ADDRESS.find(addr).and_then(|m| roster.by_sender(m.as_str())) {
            everyone.insert(s);
        }
    }
    let body = mail.readable_body().replace(INVISIBLE, "");
    for line in body.lines() {
        let Some(c) = RE_QUOTED_PARTIES.captures(line.trim()) else {
            continue;
        };
        let is_from = matches!(c[1].to_lowercase().as_str(), "from" | "από");
        let parties = &c[2];
        // Each party by its address; Exchange shows colleagues by name only
        // ("Από: Λαμπράκη Δανάη"), and then the name is all there is.
        let people: Vec<String> = parties
            .split([';', ','])
            .filter_map(|party| match RE_ADDRESS.find(party) {
                Some(m) => roster.by_sender(m.as_str()),
                None => roster.by_display_name(party),
            })
            .collect();
        if is_from {
            senders.extend(people.iter().cloned());
        }
        everyone.extend(people);
    }
    QuotedPeople { senders, everyone }
}

/// The one of `candidates` the mail's headers name, when exactly one is.
fn named_in_headers(candidates: &[String], quoted: &QuotedPeople) -> Option<String> {
    let named: Vec<&String> = candidates.iter().filter(|c| quoted.everyone.contains(*c)).collect();
    (named.len() == 1).then(|| named[0].clone())
}

static RE_OVERRIDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bSTO\s+ONOMA\s+(TIS|TOU|THS|MOU)\b(?:\s+([A-Z0-9]+))?(?:\s+([A-Z0-9]+))?").unwrap()
});
static RE_SUBJECT_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(RE|FW|FWD|AP|PRTH|PR|PROOTHISI|SCHET|TR|WG)\s*:\s*").unwrap());
static RE_SUBJECT_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(THEMATA|EPIKAIROTITA|EPIKAIROTHTA|GIA)\b\s+(?:MONTAZ\b\s*)?(.+)").unwrap());

fn subject_without_prefixes(subject_latin: &str) -> String {
    let mut s = subject_latin.to_string();
    loop {
        let next = RE_SUBJECT_PREFIX.replace(&s, "").into_owned();
        if next == s {
            return s;
        }
        s = next;
    }
}

/// Outlook's forward prefixes: English, French (TR), German (WG), and Greek
/// in both spellings: the older "ΠΡΘ:" and "Πρ:" (Προώθηση), which current
/// Outlook and Outlook on the web write. Missing "Πρ:" made a forwarded mail
/// look like a reply, and the forwarded part, links and all, was cut away
/// as the quoted original.
fn is_forward(subject: &str) -> bool {
    let s = translit(subject).to_uppercase();
    let s = s.trim_start();
    ["FW:", "FWD:", "FW :", "PRTH:", "PR:", "PR :", "PROOTHISI:", "TR:", "WG:"]
        .iter()
        .any(|p| s.starts_with(p))
}

fn resolve_journalist(
    mail: &InboundMail,
    body: &str,
    roster: &Roster,
    warnings: &mut Vec<Warning>,
) -> ResolvedJournalist {
    let mut ambiguous = false;
    // "ΓΙΑ ΕΥΗ" with two Evis on the roster: the one the forwarded headers
    // name (To/Cc) is meant.
    let quoted = quoted_people(mail, roster);

    // a. Body override.
    let body_latin = translit(body);
    if let Some(c) = RE_OVERRIDE.captures(&body_latin) {
        if &c[1] == "MOU" {
            if let Some(s) = roster.by_sender(&mail.from_address) {
                return ResolvedJournalist {
                    surname: s,
                    how: Resolution::BodyOverrideSender,
                };
            }
        } else {
            for name in [c.get(2), c.get(3)].into_iter().flatten() {
                match roster.lookup(name.as_str()) {
                    Lookup::One(s) => {
                        return ResolvedJournalist {
                            surname: s,
                            how: Resolution::BodyOverride,
                        }
                    }
                    Lookup::Ambiguous(all) => match named_in_headers(&all, &quoted) {
                        Some(s) => {
                            return ResolvedJournalist {
                                surname: s,
                                how: Resolution::BodyOverride,
                            }
                        }
                        None => ambiguous = true,
                    },
                    Lookup::None => {}
                }
            }
            let named = [c.get(2), c.get(3)]
                .into_iter()
                .flatten()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            warnings.push(Warning::new(warnings::JOURNALIST_OVERRIDE_UNKNOWN, Some(named)));
        }
    }

    // b. Subject patterns.
    let subject = subject_without_prefixes(&translit(&mail.subject));
    let mut unknown_recipient: Option<String> = None;
    if let Some(c) = RE_SUBJECT_NAME.captures(&subject) {
        for t in tokens(&c[2]) {
            match roster.lookup(t) {
                Lookup::One(s) => {
                    return ResolvedJournalist {
                        surname: s,
                        how: Resolution::Subject,
                    }
                }
                Lookup::Ambiguous(all) => match named_in_headers(&all, &quoted) {
                    Some(s) => {
                        return ResolvedJournalist {
                            surname: s,
                            how: Resolution::Subject,
                        }
                    }
                    None => ambiguous = true,
                },
                Lookup::None => {}
            }
        }
        // "ΓΙΑ ΕΥΗ" and nobody on the roster answers to it: remembered, and
        // reported below if nobody else is found either.
        if !ambiguous {
            unknown_recipient = RE_SUBJECT_RECIPIENT.captures(&mail.subject).map(|c| c[1].to_string()).filter(|w| {
                let l = translit(w);
                w.chars().count() >= 3 && !ARTICLES.contains(&l.as_str()) && !NOT_A_NAME.contains(&l.as_str())
            });
        }
    }
    // Tell MCR the name as written, so it can be added to the roster (plan
    // P4.26), when the mail did not come from a journalist on the roster.
    // Deterministic: a small model does not reliably report this itself.
    let suggest = |warnings: &mut Vec<Warning>| {
        if let Some(name) = &unknown_recipient {
            warnings.push(Warning::new(warnings::JOURNALIST_SUGGESTED, Some(name.clone())));
        }
    };

    // c. Sender address. The MCR desk is never the journalist: it passes on
    // what a journalist sent it (owner, 2026-10-05).
    if let Some(s) = roster.by_sender(&mail.from_address).filter(|s| s != "MCR") {
        return ResolvedJournalist {
            surname: s,
            how: Resolution::Sender,
        };
    }
    suggest(warnings);

    // c2. The journalist whose mail the desk (or an outsider) forwarded:
    // the newest forwarded sender who is not the desk itself.
    if let Some(s) = quoted.senders.iter().find(|s| *s != "MCR") {
        return ResolvedJournalist {
            surname: s.clone(),
            how: Resolution::Forwarded,
        };
    }

    // d. MCR.
    if ambiguous {
        warnings.push(Warning::new(warnings::JOURNALIST_AMBIGUOUS, None));
    }
    warnings.push(Warning::new(warnings::JOURNALIST_UNRESOLVED, None));
    ResolvedJournalist {
        surname: "MCR".into(),
        how: Resolution::Unresolved,
    }
}

// ---------------------------------------------------------------------------
// Body cleanup
// ---------------------------------------------------------------------------

static RE_ORIGINAL_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^-{2,}\s*(original message|αρχικό μήνυμα|forwarded message|προωθημένο μήνυμα)\s*-{2,}").unwrap()
});
/// Gmail's and Outlook's own forward marker: a forward, whatever the subject.
static RE_FORWARD_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^-{2,}\s*(forwarded message|προωθημένο μήνυμα)\s*-{2,}").unwrap()
});
static RE_WROTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(on|στις|την)\s.+(wrote|έγραψε|γράψατε)\s*:?\s*$").unwrap());
static RE_HEADER_FROM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\*?(from|από)\s*:").unwrap());
static RE_HEADER_ANY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\*?(from|sent|date|to|cc|bcc|subject|από|εστάλη|στάλθηκε|σταλθηκε|ημερομηνία|προς|κοιν\.?|θέμα)\s*:").unwrap()
});

/// Remove quoted replies and signatures.
///
/// A reply quotes what the journalist already sent, so everything below the
/// reply header is dropped. A *forward* is different: the forwarded press
/// release or colleague's mail is the content, so only the header lines go.
/// Whether `lines` hold a link the parser would queue: a video platform, a
/// news portal or a transfer. A signature's company website does not count.
fn has_media_link(lines: &[&str]) -> bool {
    let cfg = ParserConfig::default();
    lines
        .iter()
        .flat_map(|l| find_urls(l))
        .any(|(_, u)| matches!(classify(&u, &cfg), Tier::Tier1 | Tier::Tier2 | Tier::Locker))
}

/// Cut quoted history and the signature.
///
/// A forward (by subject prefix) keeps everything below the forwarded
/// header. Without a prefix, a quoted/forwarded block is still read as a
/// forward when the sender wrote no media link above it, or when it is
/// marked "Forwarded message": the sender is passing that content on, and
/// a subject edited on the way (the prefix deleted) must not lose it
/// (plan P4.23). A reply whose new text has its own links still stops at
/// the quote, so old links are not queued again.
fn strip_quotes_and_signature(body: &str, forward: bool) -> String {
    let mut forward = forward;
    let lines: Vec<&str> = body.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();

        if t == "--" {
            break; // signature delimiter ("-- ", often trimmed)
        }
        if t.starts_with('>') {
            i += 1;
            continue;
        }
        let is_block_marker = RE_ORIGINAL_MESSAGE.is_match(t) || RE_WROTE.is_match(t);
        let is_header_block = RE_HEADER_FROM.is_match(t)
            && lines[i + 1..]
                .iter()
                .take(4)
                .any(|l| RE_HEADER_ANY.is_match(l.trim()) && !RE_HEADER_FROM.is_match(l.trim()));
        if !forward && (is_block_marker || is_header_block) {
            if RE_FORWARD_MARKER.is_match(t) || !has_media_link(&out) {
                forward = true;
                i += 1;
                continue;
            }
            break;
        }
        if forward && (is_block_marker || RE_HEADER_ANY.is_match(t)) {
            i += 1;
            continue;
        }
        out.push(line);
        i += 1;
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------
// Lines, URLs, classification
// ---------------------------------------------------------------------------

/// Characters with no width: zero-width space / joiners, word joiner, BOM,
/// soft hyphen.
const INVISIBLE: [char; 6] = ['\u{200B}', '\u{200C}', '\u{200D}', '\u{2060}', '\u{FEFF}', '\u{00AD}'];

static RE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\b(?:https?://|www\.)[^\s<>"'«»\[\]{}|\\^`]+"#).unwrap());
/// A link typed without `https://` or `www.` (plan P4.11), only for hosts
/// that are nearly always a link to video or a transfer, and only with a
/// path: `youtube.com/watch?v=…`, `fb.watch/…`, `we.tl/t-…`. A bare portal
/// name in a sentence ("στο sigma.gr") is not one.
static RE_BARE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)\b(?:[a-z0-9-]+\.)*(?:youtube\.com|youtu\.be|youtube-nocookie\.com|instagram\.com|facebook\.com"#,
        r#"|fb\.watch|tiktok\.com|x\.com|twitter\.com|vimeo\.com|dailymotion\.com|dai\.ly"#,
        r#"|wetransfer\.com|we\.tl|transfernow\.net|myairbridge\.com|filemail\.com|1drv\.ms"#,
        r#"|onedrive\.live\.com|drive\.google\.com|dropbox\.com)/[^\s<>"'«»\[\]{}|\\^`]+"#
    ))
    .unwrap()
});
/// `1.`, `2)`, `3 -`, and (plan P4.15) `1ο`, `2η`, `Θέμα 3:`, `#4`, `(5)`.
static RE_NUMBERED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?i:θέμα|θεμα|thema)\s*|#\s*|\()?(\d{1,3})(?i:ος|ο|η|ον|º|°)?(\s*)([.)\-:])?(\s*)(.*)$").unwrap()
});
static RE_THEMA_LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i:θέμα|θεμα)\s*[:\-–]\s*").unwrap());
/// The word after "για" (past "μοντάζ") in a subject, in its original script.
static RE_SUBJECT_RECIPIENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:για|gia)\s+(?:(?:μοντάζ|μονταζ|montaz)\s+)?(\p{L}+)").unwrap());

/// Words that follow "για" in a subject without being a person, in Latin.
const NOT_A_NAME: &[&str] = &[
    "AVRIO", "SIMERA", "APOPSE", "TORA", "METHAVRIO", "PROI", "VRADY", "MESIMERI", "MONTAZ", "PLANA", "EKPOMPI",
    "DELTIO", "DELTIA", "PROVOLI", "METADOSI", "ARCHEIO", "EPIKAIROTITA", "THEMATA", "THEMA", "SENA", "ESENA", "SAS",
    "ESAS", "OLOUS", "OLES", "OLA", "EMAS", "ESAS", "SOU", "SAS", "LIGO", "PARAKOLOUTHISI", "ENIMEROSI", "SYNENTEFXI",
    "REPORTAZ", "VINTEO", "VIDEO", "FOTO", "FOTOGRAFIES", "SOCIAL", "SITE", "WEB", "ONLINE", "RADIO", "TV",
];

/// Greek articles and prepositions-with-article, in ELOT 743 Latin: after
/// "ΓΙΑ" they introduce a topic, not a person.
const ARTICLES: &[&str] = &["TO", "TA", "TI", "TIN", "TIS", "TON", "TOUS", "TOU", "THN", "TH", "O", "I", "OI", "ENA", "MIA", "ENAN"];

/// A journalist saying a link is video: "απόσπασμα", "βίντεο", "video",
/// "πλάνα", "ρεπορτάζ" (in ELOT 743 Latin).
static RE_VIDEO_HINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(APOSPASMA\w*|VINTEO|VIDEO|PLANA|REPORTAZ|VID)\b").unwrap());
/// A Greek-letter list: `Α.`, `Β)`, `ΣΤ.` (plan P4.15).
static RE_LETTERED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(ΣΤ|[ΑΒΓΔΕΖΗΘΙΚΛΜ])([.)])\s+(\S.*)$").unwrap());

/// Greek numerals in list order: Α Β Γ Δ Ε ΣΤ Ζ Η Θ Ι Κ Λ Μ.
fn greek_letter_value(l: &str) -> Option<u32> {
    const ORDER: [&str; 13] = ["Α", "Β", "Γ", "Δ", "Ε", "ΣΤ", "Ζ", "Η", "Θ", "Ι", "Κ", "Λ", "Μ"];
    ORDER.iter().position(|o| *o == l).map(|i| i as u32 + 1)
}

/// A lettered line whose letter is the one expected next.
fn lettered(line: &str, expected_next: u32) -> Option<(u32, String)> {
    let c = RE_LETTERED.captures(line)?;
    let n = greek_letter_value(&c[1])?;
    (n == expected_next).then(|| (n, c[3].to_string()))
}
static RE_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:GIA\s+PLANA|PLANA|VIDEO|VINTEO)\s*(?::|\s+(?:HTTP|WWW))").unwrap()
});
static RE_MARKER_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:για\s+πλάνα|gia\s+plana|πλάνα|plana|video|βίντεο|vinteo)\s*:?").unwrap());

/// First words of a line that is a greeting or sign-off, not a title.
const GREETINGS: &[&str] = &[
    "KALIMERA", "KALISPERA", "GEIA", "CHAIRETO", "CHAIRETE", "AGAPITOI", "AGAPITE", "AGAPITI",
    "EFCHARISTO", "FILIKA", "HELLO", "HI", "DEAR", "THANKS", "REGARDS", "SAS", "STELNO",
];

#[derive(Debug, Clone)]
struct Line {
    text: String,
    latin: String,
    urls: Vec<String>,
    /// Nothing but links on the line.
    url_only: bool,
}

/// Every link in `text`, in order: with a scheme or `www.`, then bare ones
/// on known hosts. Returns each match's byte range and cleaned URL.
fn find_urls(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut found: Vec<(std::ops::Range<usize>, String)> = RE_URL
        .find_iter(text)
        .flat_map(|m| split_glued(m.as_str()).into_iter().map(move |r| (m.start() + r.start)..(m.start() + r.end)))
        .filter_map(|r| clean_url(&text[r.clone()]).map(|u| (r, u)))
        .collect();
    // Blank what was found (same byte length), so a bare match cannot start
    // inside a full URL.
    let mut masked = text.to_string();
    for (r, _) in &found {
        masked.replace_range(r.clone(), &" ".repeat(r.len()));
    }
    for m in RE_BARE_URL.find_iter(&masked) {
        // `press@x.com/…`, `./youtube.com/…`: part of something else.
        let before = masked[..m.start()].chars().next_back();
        if before.is_some_and(|c| matches!(c, '@' | '.' | '/' | '-' | '_' | ':' | '=')) {
            continue;
        }
        if let Some(u) = clean_url(&format!("https://{}", m.as_str())) {
            found.push((m.range(), u));
        }
    }
    found.sort_by_key(|(r, _)| r.start);
    found
}

/// Plain-text clients hard-wrap long lines (72–78 columns), and some break a
/// long link at a `&` or `/` whatever the width. Join a line that ends in a
/// link with the next line when that line is one token of URL characters
/// and either the link ends on a separator or the line is wrap-width long
/// (plan P4.12). A next line that is a new link, a number, or has any
/// non-URL character (Greek, a space) is never joined.
fn rejoin_wrapped_urls(body: &str) -> String {
    const WRAP_WIDTH: usize = 70;
    let is_url_char = |c: char| c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c);
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        let next = line.trim();
        if let Some(prev) = out.last_mut() {
            let prev_trim = prev.trim_end();
            let ends_in_url = find_urls(prev_trim).last().is_some_and(|(r, _)| r.end == prev_trim.len());
            let continues = !next.is_empty()
                && next.chars().all(is_url_char)
                && !next.to_ascii_lowercase().starts_with("http")
                && !next.to_ascii_lowercase().starts_with("www.")
                && !next.starts_with(|c: char| c.is_ascii_digit() && next.len() <= 4);
            let at_separator = prev_trim.ends_with(['&', '=', '?', '/', '-', '_', '%', '.'])
                || next.starts_with(['&', '=', '?', '/', '#', '%']);
            let wrap_long = prev.chars().count() >= WRAP_WIDTH;
            if ends_in_url && continues && (at_separator || wrap_long) {
                let joined = format!("{prev_trim}{next}");
                *prev = joined;
                continue;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// Two links written with no space between them (`…/a1,https://…/a2`,
/// `…/a1https://…/a2`) are two links (plan P4.14). A scheme after `=`, `/`,
/// `%` or `?` is part of the first link (`?url=https://…`, web archives)
/// and does not split it.
fn split_glued(m: &str) -> Vec<std::ops::Range<usize>> {
    let lower = m.to_ascii_lowercase();
    let mut cuts = vec![0];
    for (i, _) in lower.match_indices("http") {
        if i == 0 || !(lower[i..].starts_with("http://") || lower[i..].starts_with("https://")) {
            continue;
        }
        let before = m[..i].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, ',' | ';' | '|')) {
            cuts.push(i);
        }
    }
    cuts.push(m.len());
    cuts.windows(2).map(|w| w[0]..w[1]).collect()
}

/// `text` with every link found by [`find_urls`] removed.
fn strip_urls(text: &str) -> String {
    let mut out = text.to_string();
    for (r, _) in find_urls(text).into_iter().rev() {
        out.replace_range(r, "");
    }
    out
}

impl Line {
    fn new(text: &str) -> Self {
        let urls: Vec<String> = find_urls(text).into_iter().map(|(_, u)| u).collect();
        let rest = strip_urls(text);
        let url_only = !urls.is_empty() && !rest.chars().any(|c| c.is_alphanumeric());
        Self {
            text: text.to_string(),
            latin: translit(text.trim()),
            urls,
            url_only,
        }
    }

    fn is_marker(&self) -> bool {
        RE_MARKER.is_match(&self.latin)
    }

    /// The line with links and any marker removed, if it reads as a title.
    fn title_text(&self) -> Option<String> {
        // "https://… + ΕΙΚΟΝΕΣ", "https://… + ΑΠΟΣΠΑΣΜΑ 2:28 - 3:30": after a
        // link, a "+" starts what MCR should take from it, not the story.
        let text = match find_urls(&self.text).last() {
            Some((r, _)) if self.text[r.end..].trim_start().starts_with('+') => &self.text[..r.end],
            _ => self.text.as_str(),
        };
        let no_urls = strip_urls(text);
        let no_marker = RE_MARKER_PREFIX.replace(&no_urls, "");
        let t = no_marker
            .trim()
            .trim_matches(|c: char| c == ':' || c == '-' || c == '–' || c.is_whitespace());
        if !t.chars().any(|c| c.is_alphabetic()) {
            return None;
        }
        if RE_OVERRIDE.is_match(&self.latin) {
            return None;
        }
        let first = tokens(&self.latin).next().unwrap_or("");
        if GREETINGS.contains(&first) {
            return None;
        }
        Some(t.to_string())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The real target of a redirect wrapper (plan P4.13), or `None` when `url`
/// is not one. Outlook Safe Links, Facebook / Instagram / Messenger outbound
/// links, Google's `/url`, and Proofpoint URL Defense v2 and v3.
fn unwrap_redirect(parsed: &Url, raw: &str) -> Option<String> {
    let host = parsed.host_str()?.to_ascii_lowercase();
    let param = |names: &[&str]| {
        parsed
            .query_pairs()
            .find(|(k, _)| names.contains(&k.as_ref()))
            .map(|(_, v)| v.into_owned())
            .filter(|v| v.starts_with("http://") || v.starts_with("https://") || v.starts_with("www."))
    };
    if host.ends_with("safelinks.protection.outlook.com") {
        return param(&["url"]);
    }
    if ["l.facebook.com", "lm.facebook.com", "l.instagram.com", "l.messenger.com"].contains(&host.as_str())
        && parsed.path() == "/l.php"
    {
        return param(&["u"]);
    }
    let google = host == "google.com" || host.starts_with("www.google.") || host.starts_with("google.");
    if google && parsed.path() == "/url" {
        return param(&["q", "url"]);
    }
    if host == "urldefense.com" {
        // v3: https://urldefense.com/v3/__https://real.example/path__;!!token
        let rest = raw.split_once("/v3/__")?.1;
        return rest.split_once("__;").map(|(target, _)| target.to_string());
    }
    if host == "urldefense.proofpoint.com" && parsed.path().starts_with("/v2/url") {
        // v2 encodes the target: "-" for "%", "_" for "/".
        let u = parsed.query_pairs().find(|(k, _)| k == "u")?.1.replace('-', "%").replace('_', "/");
        return Some(percent_decode(&u)).filter(|t| t.starts_with("http"));
    }
    None
}

/// Trim punctuation a sentence glued to the link, add the scheme to a bare
/// `www.` link, and unwrap redirect wrappers so the real target is queued.
fn clean_url(raw: &str) -> Option<String> {
    // Sentence punctuation, Greek included (ano teleia in both code points,
    // Greek question mark),
    // smart quotes, an ellipsis, markdown bold, and a separator left by
    // split_glued.
    const TRAILING: &[char] = &[
        '.', ',', ';', ':', '!', '?', '>', '»', '"', '\'', '…', '\u{0387}', '\u{00B7}', '\u{037E}', '”', '’', '“', '‘',
        '*', '_', '|',
    ];
    let mut u = raw.trim_start_matches(['*', '_']).trim_end_matches(TRAILING).to_string();
    // A closing parenthesis belongs to the URL only if it opened one.
    while u.ends_with(')') && u.matches(')').count() > u.matches('(').count() {
        u.pop();
        u = u.trim_end_matches(TRAILING).to_string();
    }
    if u.to_ascii_lowercase().starts_with("www.") {
        u = format!("https://{u}");
    }
    let parsed = Url::parse(&u).ok()?;
    parsed.host_str()?;
    if let Some(target) = unwrap_redirect(&parsed, &u) {
        // A wrapper inside a wrapper (Safe Links around a Facebook link).
        return clean_url(&target);
    }
    Some(u)
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Links in mail that are never a story page: a sender's own tools and
/// references, not something to put on air.
const NOT_STORY_HOSTS: &[&str] = &[
    "maps.google.com",
    "maps.app.goo.gl",
    "docs.google.com",
    "forms.gle",
    "forms.office.com",
    "calendar.google.com",
    "meet.google.com",
    "teams.microsoft.com",
    "teams.live.com",
    "zoom.us",
    "linkedin.com",
    "wikipedia.org",
    "list-manage.com",
];

/// Social sites whose one-segment pages (`facebook.com/CreteTV`) are a
/// profile, as signatures link them, not a post.
const PROFILE_HOSTS: &[&str] = &["facebook.com", "instagram.com", "x.com", "twitter.com", "tiktok.com", "threads.net"];

const DOCUMENT_EXTENSIONS: &[&str] =
    &[".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".odt", ".txt", ".rtf", ".ics", ".vcf", ".csv"];

/// Whether a link to a site the parser does not know may be a story page,
/// and so goes to the sniffer. Not: a site's front page (a signature's
/// "www.example.gr"), a document, a social profile, maps, forms, meetings,
/// encyclopaedia references.
pub fn could_be_a_story(url: &str) -> bool {
    let Ok(u) = Url::parse(url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let path = u.path().to_ascii_lowercase();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let front_page = match segments.as_slice() {
        [] => true,
        [only] => ["index.html", "index.htm", "index.php", "home", "el", "en", "gr", "default.aspx"].contains(only),
        _ => false,
    };
    if front_page && u.query().is_none() {
        return false;
    }
    if DOCUMENT_EXTENSIONS.iter().any(|e| path.ends_with(e)) {
        return false;
    }
    if NOT_STORY_HOSTS.iter().any(|d| host_matches(&host, d))
        || (host_matches(&host, "google.com") && path.starts_with("/maps"))
        || (host_matches(&host, "goo.gl") && path.starts_with("/maps"))
    {
        return false;
    }
    if PROFILE_HOSTS.iter().any(|d| host_matches(&host, d)) && segments.len() <= 1 && u.query().is_none() {
        return false;
    }
    true
}

const LOCKER_DOMAINS: &[&str] = &[
    "wetransfer.com",
    "we.tl",
    "transfernow.net",
    "myairbridge.com",
    "amna.gr",
    "filemail.com",
    "1drv.ms",
    "onedrive.live.com",
    "sharepoint.com",
    "drive.google.com",
    "dropbox.com",
];

const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".webp", ".heic"];

pub fn classify(url: &str, cfg: &ParserConfig) -> Tier {
    let Ok(parsed) = Url::parse(url) else {
        return Tier::Other;
    };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let path = parsed.path().to_ascii_lowercase();

    if LOCKER_DOMAINS.iter().any(|d| host_matches(&host, d)) {
        return Tier::Locker;
    }

    let tier1 = if ["youtube.com", "youtu.be", "youtube-nocookie.com"].iter().any(|d| host_matches(&host, d)) {
        true
    } else if host_matches(&host, "instagram.com") {
        ["/reel/", "/reels/", "/p/", "/tv/"].iter().any(|p| path.starts_with(p) || path.contains(p))
    } else if host_matches(&host, "facebook.com") {
        ["/reel/", "/watch", "/share/r/", "/share/v/", "/videos/", "video.php"]
            .iter()
            .any(|p| path.contains(p))
    } else if host_matches(&host, "fb.watch") || host_matches(&host, "tiktok.com") {
        true
    } else if host_matches(&host, "x.com") || host_matches(&host, "twitter.com") {
        path.contains("/status/")
    } else {
        ["vimeo.com", "dailymotion.com", "dai.ly"].iter().any(|d| host_matches(&host, d))
    };
    if tier1 {
        return Tier::Tier1;
    }

    if IMAGE_EXTENSIONS.iter().any(|e| path.ends_with(e)) {
        return Tier::Image;
    }
    if cfg
        .tier2_domains
        .iter()
        .any(|d| host_matches(&host, d.trim().trim_start_matches("www.").to_ascii_lowercase().as_str()))
    {
        return Tier::Tier2;
    }
    Tier::Other
}

// ---------------------------------------------------------------------------
// Keywords
// ---------------------------------------------------------------------------

const STOPWORDS: &[&str] = &[
    // Greek, in ELOT 743 form
    "KAI", "TO", "TA", "TI", "TIN", "TIS", "TOU", "TON", "TOUS", "STO", "STI", "STIN", "STON", "STA",
    "STOUS", "STIS", "ME", "GIA", "APO", "NA", "POU", "SE", "OI", "ENA", "MIA", "ENAS", "META", "KATA",
    "PROS", "OTAN", "OTI", "OLA", "EDO", "EINAI", "THEMA", "THEMATA", "VINTEO", "PLANA", "DEITE", "DEITE",
    "MONTAZ", "EPIKAIROTITA", "EPIKAIROTHTA", "LINK", "LINKS", "SYNDESMOS",
    // What to take from a link, not what it is about ("+ ΕΙΚΟΝΕΣ", "ΑΠΟΣΠΑΣΜΑ ΜΕΧΡΙ 1:25")
    "EIKONES", "EIKONA", "FOTOGRAFIES", "FOTOGRAFIA", "FOTO", "APOSPASMA", "APOSPASMATA", "MECHRI", "PLANO",
    // English
    "THE", "OF", "AND", "FOR", "WITH", "FROM", "VIDEO", "VIDEOS", "WATCH", "HERE", "NEW",
    // URL noise
    "REEL", "REELS", "SHORTS", "STATUS", "HTTPS", "HTTP", "WWW", "HTML", "PHP", "ASPX",
    // Camera / phone file names (IMG_0421.MOV, DSC01234.MP4, PXL_2026...)
    "IMG", "DSC", "MVI", "VID", "MOV", "GOPR", "GOPRO", "PXL", "DJI", "WHATSAPP", "CLIP",
];

const MAX_KEYWORD: usize = 20;

/// `[A-Z0-9]{2,20}` from free text: transliterate, drop stopwords and short
/// words, keep the first two, join.
pub fn keyword_from_text(text: &str) -> Option<String> {
    keyword_from_tokens(tokens(&translit(text)).map(|t| t.to_string()).collect(), false)
}

fn keyword_from_tokens(toks: Vec<String>, letters_only: bool) -> Option<String> {
    let picked: Vec<String> = toks
        .into_iter()
        .map(|t| t.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>())
        .filter(|t| t.len() >= 3)
        .filter(|t| !letters_only || t.chars().all(|c| c.is_ascii_alphabetic()))
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .take(2)
        .collect();
    let joined: String = picked.concat().chars().take(MAX_KEYWORD).collect();
    (joined.len() >= 2).then_some(joined)
}

/// Keyword from a human-readable URL slug (`/eimaste-oloi-mia-oikogeneia`).
/// Opaque ids (`watch?v=dQw4w9`, `/reel/C8xYz`) have no separators and are
/// skipped, so they fall through to `ASSET` rather than become gibberish.
pub fn keyword_from_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let segments: Vec<String> = parsed
        .path_segments()?
        .map(percent_decode)
        .filter(|s| !s.is_empty())
        .collect();
    for seg in segments.iter().rev() {
        let stem = seg.rsplit_once('.').map(|(a, _)| a).unwrap_or(seg);
        let parts: Vec<&str> = stem.split(['-', '_']).filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            continue;
        }
        let toks = parts.iter().map(|p| translit(p)).collect();
        if let Some(k) = keyword_from_tokens(toks, true) {
            return Some(k);
        }
    }
    None
}

fn keyword_from_filename(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name);
    let toks = tokens(&translit(stem)).map(|t| t.to_string()).collect();
    keyword_from_tokens(toks, true)
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

struct RawSection {
    index: String,
    /// Title candidate from the numbered line itself.
    header: Option<Line>,
    lines: Vec<Line>,
}

/// A numbered line: `1.`, `2)`, `3 -`, `4:`, a bare `5`, or `6 TITLE` when 6
/// is the number expected next. Times (`15:30`) and decimals (`1.5`) are not.
fn numbered(line: &str, expected_next: u32) -> Option<(u32, String)> {
    let c = RE_NUMBERED.captures(line)?;
    let n: u32 = c[1].parse().ok()?;
    let delim = c.get(3).is_some();
    // With a delimiter: is there a space after it? Without: before the title?
    let space = if delim { !c[4].is_empty() } else { !c[2].is_empty() || !c[4].is_empty() };
    // "1ο ΘΕΜΑ: ΤΙΤΛΟΣ": the word "θέμα" is the label, not the title.
    let rest = RE_THEMA_LABEL.replace(&c[5], "").into_owned();
    if delim && !space && rest.starts_with(|ch: char| ch.is_ascii_digit()) {
        return None;
    }
    if !delim {
        if rest.is_empty() {
            return Some((n, rest));
        }
        if !space || n != expected_next || !rest.starts_with(|ch: char| ch.is_alphabetic()) {
            return None;
        }
    }
    Some((n, rest))
}

fn split_sections(lines: &[Line], warnings: &mut Vec<Warning>) -> (Vec<Line>, Vec<RawSection>) {
    let mut preamble = Vec::new();
    let mut sections: Vec<RawSection> = Vec::new();
    let mut seen = HashSet::new();
    let mut expected = 1u32;
    // Letters number sections only in a mail with no digit numbering (where
    // `Α)` `Β)` are sub-items), and only once both Α and Β appear: a lone
    // "Α. Παπαδάκη" in a signature is an initial, not a list.
    let digits = lines.iter().any(|l| {
        RE_NUMBERED.captures(&l.text).is_some_and(|c| c.get(3).is_some() && !c[4].is_empty())
    });
    let letters = !digits
        && lines.iter().any(|l| lettered(&l.text, 1).is_some())
        && lines.iter().any(|l| lettered(&l.text, 2).is_some());
    for line in lines {
        let numbered_line = numbered(&line.text, expected).or_else(|| {
            if letters {
                lettered(&line.text, expected)
            } else {
                None
            }
        });
        if let Some((n, rest)) = numbered_line {
            let index = n.to_string();
            if !seen.insert(index.clone()) {
                warnings.push(Warning::new(warnings::DUPLICATE_SECTION_NUMBER, Some(index.clone())));
            }
            expected = n + 1;
            let rest_line = Line::new(&rest);
            let mut s = RawSection {
                index,
                header: Some(rest_line.clone()),
                lines: Vec::new(),
            };
            if !rest_line.urls.is_empty() || rest_line.is_marker() {
                s.lines.push(rest_line);
            }
            sections.push(s);
        } else if let Some(s) = sections.last_mut() {
            s.lines.push(line.clone());
        } else {
            preamble.push(line.clone());
        }
    }
    (preamble, sections)
}

fn section_title(s: &RawSection) -> Option<String> {
    if let Some(t) = s.header.as_ref().and_then(|h| h.title_text()) {
        return Some(t);
    }
    s.lines.iter().filter(|l| !l.is_marker()).find_map(|l| l.title_text())
}

/// URLs the journalist pointed at with `ΓΙΑ ΠΛΑΝΑ:` and friends: those on the
/// marker line plus the link-only lines right after it.
fn marked_urls(lines: &[Line], warnings: &mut Vec<Warning>, index: &str) -> HashSet<String> {
    let mut marked = HashSet::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].is_marker() {
            let before = marked.len();
            marked.extend(lines[i].urls.iter().cloned());
            let mut j = i + 1;
            while j < lines.len() && lines[j].url_only {
                marked.extend(lines[j].urls.iter().cloned());
                j += 1;
            }
            if marked.len() == before {
                warnings.push(Warning::new(warnings::MARKER_WITHOUT_URL, Some(index.to_string())));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    marked
}

fn index_suffix(i: usize) -> String {
    // A..Z, then AA, AB, ... — a 30-link listicle still gets unique indices.
    let mut n = i;
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    s
}

fn round2(x: f64) -> f64 {
    (x.clamp(0.0, 1.0) * 100.0).round() / 100.0
}

fn base_confidence(tier: Tier, marker: bool) -> f64 {
    let base = match tier {
        Tier::Tier1 => 0.95,
        Tier::Tier2 => 0.8,
        Tier::Other => 0.4,
        Tier::Attachment | Tier::Locker => 1.0,
        Tier::Image => 0.0,
    };
    if marker {
        base + 0.05
    } else {
        base
    }
}

fn status_for(tier: Tier, confidence: f64, cfg: &ParserConfig) -> JobStatus {
    match tier {
        Tier::Locker => JobStatus::ManualDownload,
        _ if confidence >= 0.7 => JobStatus::Pending,
        // Low confidence on a web link: the sniffer tries, only a failure
        // reaches review. Applies to portals as well as unknown domains, so an
        // unresolved journalist does not hold a portal link that an unknown
        // site's link would not be held for.
        Tier::Tier2 | Tier::Other if cfg.auto_attempt_unknown_domains => JobStatus::Pending,
        _ => JobStatus::RequiresReview,
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn is_video_attachment(content_type: &str, name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    content_type.to_ascii_lowercase().starts_with("video/")
        || [".mp4", ".mov", ".mxf", ".mts", ".m4v", ".avi", ".mkv"].iter().any(|e| n.ends_with(e))
}

fn is_image_attachment(content_type: &str, name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    content_type.to_ascii_lowercase().starts_with("image/") || IMAGE_EXTENSIONS.iter().any(|e| n.ends_with(e))
}

/// URL used for a job fed by an email attachment (plan P4.6). Unique per
/// message and attachment, so dedup works on it like on any other URL.
pub fn attachment_url(internet_message_id: &str, attachment_id: &str) -> String {
    let mid = internet_message_id.trim().trim_start_matches('<').trim_end_matches('>');
    format!("attachment://{mid}/{attachment_id}")
}

static RE_SUBJECT_PREFIX_ORIGINAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(RE|FW|FWD|ΑΠ|ΠΡΘ|ΠΡ|ΠΡΟΩΘΗΣΗ|ΣΧΕΤ|TR|WG)\s*:\s*").unwrap());

/// Title and keyword from the subject of an unnumbered email: the title is
/// the subject as written (minus `RE:`/`FW:`), the keyword leaves out routing
/// words and the journalist's own name. `None` if nothing usable remains.
fn subject_title(subject: &str, roster: &Roster, journalist: &str) -> Option<(String, String)> {
    let latin = subject_without_prefixes(&translit(subject));
    // "… ΓΙΑ ΕΥΗ", "ΓΙΑ ΜΟΝΤΑΖ ΓΙΩΡΓΟ": the word after ΓΙΑ (past ΜΟΝΤΑΖ) names
    // who the mail is for, not the story, whether or not the roster knows
    // them. After an article ("ΓΙΑ ΤΟ ΣΕΙΣΜΟ") it is the story (plan P4.26).
    let all: Vec<&str> = tokens(&latin).collect();
    let mut recipient_at = HashSet::new();
    for (i, t) in all.iter().enumerate() {
        if *t == "GIA" {
            let mut k = i + 1;
            if all.get(k) == Some(&"MONTAZ") {
                k += 1;
            }
            if let Some(next) = all.get(k) {
                if !ARTICLES.contains(next) {
                    recipient_at.insert(k);
                }
            }
        }
    }
    let kept: Vec<&str> = all
        .iter()
        .enumerate()
        .filter(|(i, _)| !recipient_at.contains(i))
        .map(|(_, t)| *t)
        .filter(|t| !matches!(roster.lookup(t), Lookup::One(ref s) if s == journalist))
        .filter(|t| !["THEMATA", "EPIKAIROTITA", "EPIKAIROTHTA", "GIA", "MONTAZ", "EKTAKTO", "BREAKING", "URGENT"].contains(t))
        .collect();
    // "Για Διαμαντη" and nothing else: a name the roster does not know is
    // as likely the story as a person, and it is the only word the subject
    // has. Better than the first line of the body, which in a forward is
    // a header ("Στάλθηκε: Τετάρτη" became the keyword).
    let unlisted_recipient = || {
        let words: Vec<&str> = recipient_at
            .iter()
            .filter_map(|i| all.get(*i).copied())
            .filter(|t| matches!(roster.lookup(t), Lookup::None))
            .filter(|t| !NOT_A_NAME.contains(t))
            .collect();
        keyword_from_text(&words.join(" "))
    };
    let keyword = keyword_from_text(&kept.join(" ")).or_else(unlisted_recipient)?;
    let mut display = subject.trim().to_string();
    loop {
        let next = RE_SUBJECT_PREFIX_ORIGINAL.replace(&display, "").into_owned();
        if next == display {
            break;
        }
        display = next;
    }
    Some((display, keyword))
}

/// The body the parser reads: invisible characters, quoted replies and the
/// signature removed, wrapped links joined, poison links stripped.
static RE_CID_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\[cid:[^\]\s]+\]").unwrap());

pub fn cleaned_body(mail: &InboundMail) -> String {
    // Zero-width and soft-hyphen characters come along when a link is copied
    // out of a chat app or a web page, and split the URL.
    let body = mail.readable_body().replace(INVISIBLE, "");
    // Outlook's text part stands "[cid:…]" where an inline image (a
    // signature logo) was; it became a section's title and keyword.
    let body = RE_CID_PLACEHOLDER.replace_all(&body, "");
    let body = strip_quotes_and_signature(&body, is_forward(&mail.subject));
    let body = rejoin_wrapped_urls(&body);
    decontaminate_email_body(&body)
}

pub fn parse(mail: &InboundMail, roster: &[Journalist], cfg: &ParserConfig) -> ParsedEmail {
    let mut warnings = Vec::new();
    let roster_ix = Roster::new(roster);

    // 1. Cleanup.
    let body = cleaned_body(mail);

    // 2. Journalist.
    let journalist = resolve_journalist(mail, &body, &roster_ix, &mut warnings);
    let unresolved = journalist.how == Resolution::Unresolved;

    let urgent = {
        let s = translit(&mail.subject);
        cfg.urgent_keywords.iter().any(|k| {
            let k = translit(k);
            !k.is_empty() && tokens(&s).any(|t| t == k)
        })
    };

    // 3. Sections.
    let lines: Vec<Line> = body.lines().map(Line::new).collect();
    let (preamble, mut raw) = split_sections(&lines, &mut warnings);
    // (title shown to people, keyword source) per section.
    let with_keyword = |t: Option<String>| {
        let k = t.as_deref().and_then(keyword_from_text);
        (t, k)
    };
    let mut titles: Vec<(Option<String>, Option<String>)> =
        raw.iter().map(|s| with_keyword(section_title(s))).collect();

    if raw.is_empty() {
        let title = match subject_title(&mail.subject, &roster_ix, &journalist.surname) {
            Some((t, k)) => (Some(t), Some(k)),
            None => with_keyword(preamble.iter().filter(|l| !l.is_marker()).find_map(|l| l.title_text())),
        };
        raw.push(RawSection {
            index: "1".into(),
            header: None,
            lines: preamble,
        });
        titles = vec![title];
    } else if preamble.iter().any(|l| !l.urls.is_empty()) {
        warnings.push(Warning::new(warnings::PREAMBLE_URLS, None));
        let first = &mut raw[0];
        let mut merged: Vec<Line> = preamble.into_iter().filter(|l| !l.urls.is_empty()).collect();
        merged.append(&mut first.lines);
        first.lines = merged;
    }

    // 4.–7. URLs → jobs.
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut ignored_urls = Vec::new();
    let mut images = 0usize;
    let mut sections = Vec::new();

    for (s, (title, section_kw)) in raw.iter().zip(titles) {
        let marked = marked_urls(&s.lines, &mut warnings, &s.index);
        let mut candidates: Vec<(String, Tier, bool)> = Vec::new();
        // Links a journalist called video ("απόσπασμα", "βίντεο" on the same
        // line or the line above): a portal page among them is more likely
        // to carry a player (plan P4.24).
        let mut video_hinted: HashSet<String> = HashSet::new();
        for (li, line) in s.lines.iter().enumerate() {
            let hinted = RE_VIDEO_HINT.is_match(&line.latin)
                || (li > 0 && s.lines[li - 1].urls.is_empty() && RE_VIDEO_HINT.is_match(&s.lines[li - 1].latin));
            for u in &line.urls {
                if !seen_urls.insert(urlnorm::normalize(u)) {
                    continue;
                }
                if hinted {
                    video_hinted.insert(u.clone());
                }
                candidates.push((u.clone(), classify(u, cfg), marked.contains(u)));
            }
        }

        // Video platforms *and* news portals are queued: journalists send
        // article links so the video embedded in the page is downloaded
        // (owner, plan P4.24), and so is a site we do not know (owner,
        // 2026-10-05): the sniffer tries it as it would a portal, and a page
        // without a video goes to review instead of being dropped unseen.
        // A "ΓΙΑ ΠΛΑΝΑ:" marker still narrows to what it marks.
        let any_marked = candidates.iter().any(|(_, t, m)| *m && !matches!(t, Tier::Locker | Tier::Image));

        let mut selected: Vec<(String, Tier, bool)> = Vec::new();
        for (u, tier, m) in candidates {
            let take = match tier {
                Tier::Locker => true,
                Tier::Image => false,
                _ if any_marked => m,
                Tier::Tier1 | Tier::Tier2 => true,
                // The journalist said it is video ("+ ΠΛΑΝΑ"): always tried.
                _ if video_hinted.contains(&u) => true,
                _ => could_be_a_story(&u),
            };
            if tier == Tier::Image {
                images += 1;
            }
            if take {
                selected.push((u, tier, m));
            } else {
                ignored_urls.push(u);
            }
        }

        if selected.is_empty() && s.header.is_some() {
            warnings.push(Warning::new(warnings::SECTION_WITHOUT_LINKS, Some(s.index.clone())));
        }

        let many = selected.len() > 1;
        let jobs = selected
            .into_iter()
            .enumerate()
            .map(|(i, (url, tier, marker))| {
                let penalty = if unresolved { 0.2 } else { 0.0 };
                let hint = if tier == Tier::Tier2 && video_hinted.contains(&url) { 0.1 } else { 0.0 };
                let confidence = round2(base_confidence(tier, marker) + hint - penalty);
                let keyword = section_kw
                    .clone()
                    .or_else(|| keyword_from_url(&url))
                    .unwrap_or_else(|| "ASSET".into());
                ParsedJob {
                    index_str: if many {
                        format!("{}{}", s.index, index_suffix(i))
                    } else {
                        s.index.clone()
                    },
                    status: status_for(tier, confidence, cfg),
                    url,
                    tier,
                    keyword,
                    confidence,
                    marker,
                    attachment_id: None,
                }
            })
            .collect();

        sections.push(Section {
            index_str: s.index.clone(),
            title,
            keyword: section_kw,
            jobs,
        });
    }

    // P4.6: video attachments are assets of their own.
    let max_bytes = cfg.max_attachment_mb.saturating_mul(1024 * 1024);
    let mut att_jobs = Vec::new();
    for a in &mail.attachments {
        if is_video_attachment(&a.content_type, &a.name) {
            if a.size > max_bytes {
                warnings.push(Warning::new(warnings::ATTACHMENT_TOO_LARGE, Some(a.name.clone())));
                continue;
            }
            att_jobs.push(a);
        } else if is_image_attachment(&a.content_type, &a.name) {
            images += 1;
        }
    }
    if !att_jobs.is_empty() {
        let has_jobs = sections.iter().any(|s| !s.jobs.is_empty());
        // Attachments get the next number after the last written one; in an
        // email with no links they are section 1 (replacing the empty one).
        let index = if has_jobs {
            let max = sections.iter().filter_map(|s| s.index_str.parse::<u32>().ok()).max().unwrap_or(0);
            (max + 1).to_string()
        } else {
            let (title, keyword) = sections
                .first()
                .map(|s| (s.title.clone(), s.keyword.clone()))
                .unwrap_or_default();
            sections.retain(|s| !s.jobs.is_empty());
            warnings.retain(|w| w.code != warnings::SECTION_WITHOUT_LINKS);
            sections.push(Section {
                index_str: "1".into(),
                title,
                keyword,
                jobs: Vec::new(),
            });
            "1".into()
        };
        if has_jobs {
            sections.push(Section {
                index_str: index.clone(),
                title: None,
                keyword: None,
                jobs: Vec::new(),
            });
        }
        let section = sections.last_mut().unwrap();
        let section_kw = section.keyword.clone();
        let many = att_jobs.len() > 1;
        for (i, a) in att_jobs.into_iter().enumerate() {
            let penalty = if unresolved { 0.2 } else { 0.0 };
            let confidence = round2(base_confidence(Tier::Attachment, false) - penalty);
            section.jobs.push(ParsedJob {
                index_str: if many {
                    format!("{index}{}", index_suffix(i))
                } else {
                    index.clone()
                },
                url: attachment_url(&mail.internet_message_id, &a.id),
                tier: Tier::Attachment,
                keyword: keyword_from_filename(&a.name)
                    .or_else(|| section_kw.clone())
                    .unwrap_or_else(|| "ASSET".into()),
                confidence,
                status: status_for(Tier::Attachment, confidence, cfg),
                marker: false,
                attachment_id: Some(a.id.clone()),
            });
        }
    }

    let job_count: usize = sections.iter().map(|s| s.jobs.len()).sum();
    let outcome = if job_count > 0 {
        Outcome::Jobs
    } else if images > 0 {
        Outcome::PhotosOnly
    } else {
        Outcome::NoLinks
    };
    if outcome != Outcome::Jobs {
        // "Section 1 has no links" says nothing an empty result does not.
        warnings.retain(|w| w.code != warnings::SECTION_WITHOUT_LINKS);
    }

    ParsedEmail {
        journalist,
        group: None,
        outcome,
        urgent,
        sections,
        ignored_urls,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            j("MCR", "Master Control Room", &[], &[]),
            j("PAPADAKI", "Anna Papadaki", &["a.papadaki@example.gr"], &["ΠΑΠΑΔΑΚΗ"]),
            j("NIKOLAOU", "Giorgos Nikolaou", &["g.nikolaou@example.gr"], &["ΝΙΚΟΛΑΟΣ"]),
            j("GEORGIOU", "Anna Georgiou", &[], &[]),
        ]
    }

    fn mail(subject: &str, from: &str, body: &str) -> InboundMail {
        InboundMail {
            subject: subject.into(),
            from_address: from.into(),
            body_text: body.into(),
            internet_message_id: "<m1@example.gr>".into(),
            ..Default::default()
        }
    }

    #[test]
    fn genitive_and_first_names_resolve() {
        let r = roster();
        let ix = Roster::new(&r);
        assert!(matches!(ix.lookup("ΠΑΠΑΔΑΚΗ"), Lookup::One(ref s) if s == "PAPADAKI"));
        assert!(matches!(ix.lookup("Παπαδάκης"), Lookup::One(ref s) if s == "PAPADAKI"));
        assert!(matches!(ix.lookup("ΝΙΚΟΛΑΟΥ"), Lookup::One(ref s) if s == "NIKOLAOU"));
        assert!(matches!(ix.lookup("Γιώργου"), Lookup::None));
        // Two staff called Anna: the first name alone must not pick one.
        assert!(matches!(ix.lookup("ΑΝΝΑΣ"), Lookup::Ambiguous(_)));
        assert!(matches!(ix.lookup("ΤΟ"), Lookup::None));
    }

    /// The MCR desk on the roster by its addresses, as in the station's roster.
    fn desk_roster() -> Vec<Journalist> {
        let mut r = roster();
        r[0].emails = vec!["master@example.gr".into(), "flow@example.gr".into()];
        r
    }

    fn resolved(m: &InboundMail, r: &[Journalist]) -> (String, Resolution) {
        let p = parse(m, r, &ParserConfig::default());
        (p.journalist.surname, p.journalist.how)
    }

    /// "VIRAL ΓΙΑ ΕΥΗ" relayed by the desk (2026-10-05): two Evis on the
    /// roster, and the forwarded mail had one of them on its Cc line.
    #[test]
    fn a_first_name_two_people_share_is_settled_by_the_forwarded_headers() {
        let body = "Από: Master Desk <master@example.gr>\n\
                    Προς: Flow Desk <flow@example.gr>\n\
                    Από: Giorgos Nikolaou <g.nikolaou@example.gr>\n\
                    Προς: Master Desk <master@example.gr>\n\
                    Κοιν.: Άννα Γεωργίου; Graphics <graphics@example.gr>\n\
                    https://www.youtube.com/watch?v=abcdefghijk";
        let m = mail("VIRAL ΓΙΑ ΑΝΝΑ", "flow@example.gr", body);
        assert_eq!(resolved(&m, &desk_roster()), ("GEORGIOU".into(), Resolution::Subject));

        // Neither Anna in the headers: still ambiguous, and the desk is not
        // the answer; the journalist who forwarded it is.
        let m = mail("VIRAL ΓΙΑ ΑΝΝΑ", "flow@example.gr", &body.replace("Κοιν.: Άννα Γεωργίου; ", "Κοιν.: "));
        assert_eq!(resolved(&m, &desk_roster()), ("NIKOLAOU".into(), Resolution::Forwarded));
    }

    #[test]
    fn the_desk_is_never_the_journalist() {
        let link = "https://www.youtube.com/watch?v=abcdefghijk";
        // The desk naming nobody: unresolved (for MCR to place), not MCR by sender.
        let m = mail("VIRAL", "flow@example.gr", link);
        assert_eq!(resolved(&m, &desk_roster()), ("MCR".into(), Resolution::Unresolved));

        // Exchange shows a colleague by name only, surname first.
        let m = mail("Πρ: VIRAL", "flow@example.gr", &format!("Από: Νικολάου Giorgos\nΣτάλθηκε: Τετάρτη\nΘέμα: VIRAL\n{link}"));
        assert_eq!(resolved(&m, &desk_roster()), ("NIKOLAOU".into(), Resolution::Forwarded));

        // A journalist writing in person is still the journalist, whoever
        // they quote.
        let m = mail("Πρ: VIRAL", "a.papadaki@example.gr", &format!("From: Giorgos Nikolaou <g.nikolaou@example.gr>\nSent: Wednesday\n{link}"));
        assert_eq!(resolved(&m, &desk_roster()), ("PAPADAKI".into(), Resolution::Sender));
    }

    /// Owner, 2026-10-05: a news site that is not on the portal list is still
    /// downloaded, next to known links too; only links that cannot be a
    /// story page are left out.
    #[test]
    fn links_to_unknown_sites_are_queued_unless_they_cannot_be_a_story() {
        for story in [
            "https://www.bovary.gr/people-and-style/glam-stars/tzoni-ntep-entyposiaki-metamorfosi",
            "https://www.example-news.gr/article.php?id=4411",
            "https://www.facebook.com/permalink.php?story_fbid=1&id=2",
            "https://t.me/somechannel/1234",
        ] {
            assert!(could_be_a_story(story), "{story}");
        }
        for not in [
            "https://www.example.gr/",
            "https://www.example.gr/index.html",
            "https://www.example.gr/el/",
            "https://www.example.org/press/deltio.pdf",
            "https://www.facebook.com/CreteTV",
            "https://www.instagram.com/cretetv/",
            "https://el.wikipedia.org/wiki/Σητεία",
            "https://www.google.com/maps/place/Heraklion",
            "https://maps.app.goo.gl/abc123",
            "https://teams.microsoft.com/l/meetup-join/19%3a",
            "https://docs.google.com/document/d/1/edit",
            "mailto:desk@example.gr",
        ] {
            assert!(!could_be_a_story(not), "{not}");
        }

        // In the mail: a known video link and an unknown portal in one
        // section, plus a signature front page.
        let body = "ΣΕΙΣΜΟΣ ΣΤΗΝ ΚΡΗΤΗ\nhttps://www.youtube.com/watch?v=abcdefghijk\nhttps://www.example-news.gr/kriti/seismos-4-2-rihter\n\nwww.example.gr";
        let m = mail("ΣΕΙΣΜΟΣ", "a.papadaki@example.gr", body);
        let p = parse(&m, &roster(), &ParserConfig::default());
        let queued: Vec<(&str, JobStatus)> = p.jobs().map(|j| (j.url.as_str(), j.status)).collect();
        assert_eq!(
            queued,
            vec![
                ("https://www.youtube.com/watch?v=abcdefghijk", JobStatus::Pending),
                ("https://www.example-news.gr/kriti/seismos-4-2-rihter", JobStatus::Pending),
            ]
        );
        assert_eq!(p.ignored_urls, vec!["https://www.example.gr".to_string()]);
    }

    /// Outlook's text part, 2026-10-05: the signature logo's "[cid:…]" was
    /// the last section's title, and its keyword CID475509D3.
    #[test]
    fn an_inline_image_placeholder_is_not_a_title() {
        let m = mail(
            "VIRAL",
            "a.papadaki@example.gr",
            "1.\nhttps://www.youtube.com/watch?v=abcdefghijk\n2.\nhttps://www.youtube.com/watch?v=bbcdefghijk\n\n[cid:475509d3-01d5-480c-80b4-e49e9ce41fb5]\n",
        );
        let p = parse(&m, &roster(), &ParserConfig::default());
        assert!(p.sections.iter().all(|s| s.title.is_none() && s.keyword.is_none()), "{:?}", p.sections);
    }

    /// Greek Outlook's "Στάλθηκε:" and "Κοιν.:" were not known as header
    /// lines, and the forward's date became the story keyword.
    #[test]
    fn greek_outlook_forward_headers_are_not_content() {
        let body = "Από: Giorgos Nikolaou <g.nikolaou@example.gr>\n\
                    Στάλθηκε: Τετάρτη 30 Σεπτεμβρίου 2026 1:04:12 μ.μ.\n\
                    Προς: Master Desk <master@example.gr>\n\
                    Κοιν.: Anna Georgiou <a.georgiou@example.gr>\n\
                    Θέμα: Fw: ΣΕΙΣΜΟΣ\n\
                    https://www.youtube.com/watch?v=abcdefghijk";
        let cleaned = strip_quotes_and_signature(body, true);
        assert_eq!(cleaned, "https://www.youtube.com/watch?v=abcdefghijk");
    }

    #[test]
    fn numbered_lines_exclude_times_and_decimals() {
        assert_eq!(numbered("1. ΤΙΤΛΟΣ", 1), Some((1, "ΤΙΤΛΟΣ".into())));
        assert_eq!(numbered("2)", 1), Some((2, "".into())));
        assert_eq!(numbered("3 - ΘΕΜΑ", 1), Some((3, "ΘΕΜΑ".into())));
        assert_eq!(numbered("15:30 συνέντευξη", 1), None);
        assert_eq!(numbered("1.5 εκατ. ευρώ", 1), None);
        assert_eq!(numbered("2024 ΕΚΛΟΓΕΣ", 1), None);
        // Bare "N TITLE" only as the next expected number.
        assert_eq!(numbered("1 ΠΑΡΕΛΑΣΗ", 1), Some((1, "ΠΑΡΕΛΑΣΗ".into())));
        assert_eq!(numbered("3 νεκροί σε τροχαίο", 1), None);
        assert_eq!(numbered("1. 2024: χρονιά ρεκόρ", 1), Some((1, "2024: χρονιά ρεκόρ".into())));
    }

    #[test]
    fn classification() {
        let c = ParserConfig::default();
        assert_eq!(classify("https://www.youtube.com/watch?v=x", &c), Tier::Tier1);
        assert_eq!(classify("https://www.instagram.com/reel/abc/", &c), Tier::Tier1);
        assert_eq!(classify("https://www.instagram.com/someone/", &c), Tier::Other);
        assert_eq!(classify("https://www.facebook.com/share/r/abc/", &c), Tier::Tier1);
        assert_eq!(classify("https://x.com/user/status/123", &c), Tier::Tier1);
        assert_eq!(classify("https://x.com/user", &c), Tier::Other);
        assert_eq!(classify("https://www.neakriti.gr/article/1", &c), Tier::Tier2);
        assert_eq!(classify("https://www.neakriti.gr/img/1.jpg", &c), Tier::Image);
        assert_eq!(classify("https://we.tl/t-abc", &c), Tier::Locker);
        assert_eq!(classify("https://1drv.ms/v/s!abc", &c), Tier::Locker);
        assert_eq!(classify("https://example.org/story", &c), Tier::Other);
        // Not fooled by a lookalike host.
        assert_eq!(classify("https://notyoutube.com/watch?v=x", &c), Tier::Other);
    }

    #[test]
    fn a_quote_is_read_as_a_forward_only_when_nothing_above_it_is_a_media_link() {
        // Gmail's marker is a forward even with a link of the sender's own above it.
        let gmail = "Και αυτό: https://youtu.be/mine
---------- Forwarded message ---------
From: X <x@example.org>
Date: Wed
Subject: y

https://youtu.be/theirs";
        let kept = strip_quotes_and_signature(gmail, false);
        assert!(kept.contains("youtu.be/mine") && kept.contains("youtu.be/theirs"), "{kept}");
        assert!(!kept.contains("From: X"), "{kept}");

        // A reply with its own link keeps stopping at the quote.
        let reply = "Ξέχασα ένα: https://youtu.be/new
From: A <a@example.gr>
Sent: Tuesday
Subject: old

https://youtu.be/old";
        let kept = strip_quotes_and_signature(reply, false);
        assert!(kept.contains("youtu.be/new") && !kept.contains("youtu.be/old"), "{kept}");

        // Only a signature website above: the quoted part is what was sent on.
        let fwd = "Δες αυτό
www.example.gr
Από: A <a@example.org>
Εστάλη: Τετάρτη
Θέμα: b

https://youtu.be/sent-on";
        assert!(strip_quotes_and_signature(fwd, false).contains("youtu.be/sent-on"));
    }

    #[test]
    fn a_model_answering_nothing_as_a_word_gets_no_keyword() {
        for nothing in ["null", "NULL", "None", "N/A", "undefined", "keyword", "asset"] {
            assert_eq!(valid_keyword(nothing), None, "{nothing}");
        }
        assert_eq!(valid_keyword("λιμάνι").as_deref(), Some("LIMANI"));
    }

    #[test]
    fn links_come_out_of_brackets_quotes_markdown_and_greek_punctuation() {
        let urls = |line: &str| find_urls(line).into_iter().map(|(_, u)| u).collect::<Vec<_>>();
        let cases: &[(&str, &[&str])] = &[
            ("<https://youtu.be/w1>", &["https://youtu.be/w1"]),
            ("(δείτε https://youtu.be/w2)", &["https://youtu.be/w2"]),
            ("[Δείτε το](https://youtu.be/w3)", &["https://youtu.be/w3"]),
            ("**https://youtu.be/w4**", &["https://youtu.be/w4"]),
            ("Εδώ: https://youtu.be/w5…", &["https://youtu.be/w5"]),
            ("το βίντεο https://youtu.be/w6· και", &["https://youtu.be/w6"]),
            ("το είδες https://youtu.be/w7\u{037E}", &["https://youtu.be/w7"]),
            ("“https://youtu.be/w8”", &["https://youtu.be/w8"]),
            ("https://youtu.be/w9!!", &["https://youtu.be/w9"]),
            ("https://en.wikipedia.org/wiki/Knossos_(palace)", &["https://en.wikipedia.org/wiki/Knossos_(palace)"]),
            ("https://youtu.be/w10,https://youtu.be/w11", &["https://youtu.be/w10", "https://youtu.be/w11"]),
            ("https://youtu.be/w12https://youtu.be/w13", &["https://youtu.be/w12", "https://youtu.be/w13"]),
            ("https://youtu.be/w14 | https://youtu.be/w15", &["https://youtu.be/w14", "https://youtu.be/w15"]),
            (
                "https://web.archive.org/web/2026/https://www.ertnews.gr/video/1",
                &["https://web.archive.org/web/2026/https://www.ertnews.gr/video/1"],
            ),
            ("https://example.gr/r?next=https://youtu.be/w16", &["https://example.gr/r?next=https://youtu.be/w16"]),
        ];
        for (line, want) in cases {
            assert_eq!(urls(line), want.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "{line}");
        }
    }

    #[test]
    fn redirect_wrappers_are_unwrapped_to_the_real_link() {
        let cases = [
            (
                "https://l.facebook.com/l.php?u=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Dfb1&h=AT0x",
                "https://www.youtube.com/watch?v=fb1",
            ),
            ("https://www.google.com/url?q=https://youtu.be/g1&sa=D&ust=1", "https://youtu.be/g1"),
            ("https://www.google.gr/url?sa=t&url=https%3A%2F%2Fvimeo.com%2F42", "https://vimeo.com/42"),
            (
                "https://urldefense.com/v3/__https://www.youtube.com/watch?v=pp3__;!!Ab12Cd!xyz$",
                "https://www.youtube.com/watch?v=pp3",
            ),
            (
                "https://urldefense.proofpoint.com/v2/url?u=https-3A__youtu.be_pp2&d=DwMF&c=x",
                "https://youtu.be/pp2",
            ),
            (
                "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fl.facebook.com%2Fl.php%3Fu%3Dhttps%253A%252F%252Fyoutu.be%252Fnested&data=05",
                "https://youtu.be/nested",
            ),
        ];
        for (wrapped, real) in cases {
            assert_eq!(clean_url(wrapped).as_deref(), Some(real), "{wrapped}");
        }
        // Not wrappers: left alone.
        assert_eq!(
            clean_url("https://www.google.com/maps/place/Heraklion").as_deref(),
            Some("https://www.google.com/maps/place/Heraklion")
        );
        assert_eq!(
            clean_url("https://www.facebook.com/watch/?v=123").as_deref(),
            Some("https://www.facebook.com/watch/?v=123")
        );
        // A wrapper whose target is not a web link stays as it is.
        assert_eq!(
            clean_url("https://www.google.com/url?q=javascript:alert(1)").as_deref(),
            Some("https://www.google.com/url?q=javascript:alert(1)")
        );
    }

    #[test]
    fn urls_lose_trailing_punctuation_and_safelinks() {
        assert_eq!(clean_url("https://youtu.be/abc).").as_deref(), Some("https://youtu.be/abc"));
        assert_eq!(
            clean_url("https://en.wikipedia.org/wiki/Crete_(island)").as_deref(),
            Some("https://en.wikipedia.org/wiki/Crete_(island)")
        );
        assert_eq!(clean_url("www.lifo.gr/a,").as_deref(), Some("https://www.lifo.gr/a"));
        let safe = "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fyoutu.be%2Fabc&data=05%7C01";
        assert_eq!(clean_url(safe).as_deref(), Some("https://youtu.be/abc"));
    }

    #[test]
    fn keywords() {
        assert_eq!(keyword_from_text("ΠΑΡΕΛΑΣΗ ΣΤΟ ΗΡΑΚΛΕΙΟ").as_deref(), Some("PARELASIIRAKLEIO"));
        assert_eq!(keyword_from_text("Δείτε το βίντεο").as_deref(), None);
        assert_eq!(
            keyword_from_url("https://www.lifo.gr/now/sport/eimaste-oloi-mia-oikogeneia").as_deref(),
            Some("EIMASTEOLOI")
        );
        assert_eq!(keyword_from_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), None);
        assert!(keyword_from_text("Συνέντευξη του περιφερειάρχη Κρήτης για τον καύσωνα").unwrap().len() <= 20);
    }

    #[test]
    fn index_suffixes_do_not_collide_past_z() {
        assert_eq!(index_suffix(0), "A");
        assert_eq!(index_suffix(25), "Z");
        assert_eq!(index_suffix(26), "AA");
        assert_eq!(index_suffix(27), "AB");
    }

    #[test]
    fn reply_quotes_are_dropped_but_forwards_kept() {
        let body = "1. ΝΕΟ\nhttps://youtu.be/new1\n\nFrom: Someone\nSent: Monday\nTo: ingest\n\n1. ΠΑΛΙΟ\nhttps://youtu.be/old1";
        let reply = parse(&mail("RE: ΘΕΜΑΤΑ", "a.papadaki@example.gr", body), &roster(), &ParserConfig::default());
        let urls: Vec<_> = reply.jobs().map(|j| j.url.as_str()).collect();
        assert_eq!(urls, vec!["https://youtu.be/new1"]);

        let fwd = parse(&mail("FW: ΘΕΜΑΤΑ", "a.papadaki@example.gr", body), &roster(), &ParserConfig::default());
        assert_eq!(fwd.jobs().count(), 2);
    }

    #[test]
    fn unresolved_journalist_lowers_confidence_but_not_below_pending_for_tier1() {
        let p = parse(&mail("Βίντεο", "stranger@example.org", "https://youtu.be/abc"), &roster(), &ParserConfig::default());
        assert_eq!(p.journalist.surname, "MCR");
        let j = p.jobs().next().unwrap();
        assert_eq!(j.confidence, 0.75);
        assert_eq!(j.status, JobStatus::Pending);
        assert!(p.has_warning(warnings::JOURNALIST_UNRESOLVED));
    }

    #[test]
    fn unknown_domains_go_to_review_when_auto_attempt_is_off() {
        let cfg = ParserConfig {
            auto_attempt_unknown_domains: false,
            ..Default::default()
        };
        let p = parse(&mail("x", "a.papadaki@example.gr", "https://example.org/story"), &roster(), &cfg);
        assert_eq!(p.jobs().next().unwrap().status, JobStatus::RequiresReview);
        let p = parse(&mail("x", "a.papadaki@example.gr", "https://example.org/story"), &roster(), &ParserConfig::default());
        assert_eq!(p.jobs().next().unwrap().status, JobStatus::Pending);
    }
}

// ---------------------------------------------------------------------------
// Hooks for the LLM assist (plan P4.4). Everything the model proposes passes
// through one of these; none of them can add a URL or an index.
// ---------------------------------------------------------------------------

/// The roster surname a proposed name refers to, under the same rules the
/// parser uses (aliases, genitive prefixes, ambiguity).
///
/// Every word must name the same journalist: `PAPADAKI` and `Anna Papadaki`
/// pass, `ANNA; DROP TABLE` does not — a roster name buried in other text is
/// not an answer. `None` for nobody, several people, or `MCR`.
pub fn resolve_name(roster: &[Journalist], name: &str) -> Option<String> {
    let ix = Roster::new(roster);
    let words: Vec<&str> = tokens(name).collect();
    if words.is_empty() || words.len() > 3 {
        return None;
    }
    let mut found: Option<String> = None;
    for w in words {
        let Lookup::One(s) = ix.lookup(w) else {
            return None;
        };
        match &found {
            Some(prev) if *prev != s => return None,
            _ => found = Some(s),
        }
    }
    found.filter(|s| s != "MCR")
}

static RE_KEYWORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z0-9]{2,20}$").unwrap());

/// A keyword as the LLM proposed it, if it is one: transliterated, and then
/// exactly `^[A-Z0-9]{2,20}$` — no spaces, no punctuation, nothing trimmed
/// into shape.
pub fn valid_keyword(raw: &str) -> Option<String> {
    let k = translit(raw.trim());
    // A model that has no keyword sometimes answers the word for "nothing"
    // as a string; "NULL" would pass the pattern and name a file.
    const NOTHING: &[&str] = &["NULL", "NONE", "NIL", "NA", "N/A", "UNDEFINED", "UNKNOWN", "EMPTY", "KEYWORD", "ASSET"];
    (RE_KEYWORD.is_match(&k) && !STOPWORDS.contains(&k.as_str()) && !NOTHING.contains(&k.as_str())).then_some(k)
}

/// Adopt a journalist for a mail the parser left unresolved: the −0.2
/// confidence penalty is taken back and every job's status recomputed.
pub fn adopt_journalist(parsed: &mut ParsedEmail, surname: String, cfg: &ParserConfig) {
    // Unresolved, or only the MCR desk by sender address (plan P4.26).
    let was_unresolved = parsed.journalist.how == Resolution::Unresolved;
    if !was_unresolved && parsed.journalist.surname != "MCR" {
        return;
    }
    parsed.journalist = ResolvedJournalist {
        surname,
        how: Resolution::LlmAssist,
    };
    parsed
        .warnings
        .retain(|w| w.code != warnings::JOURNALIST_UNRESOLVED && w.code != warnings::JOURNALIST_AMBIGUOUS);
    if !was_unresolved {
        return; // no unresolved-journalist penalty was taken
    }
    for s in &mut parsed.sections {
        for j in &mut s.jobs {
            j.confidence = round2(j.confidence + 0.2);
            j.status = status_for(j.tier, j.confidence, cfg);
        }
    }
}

/// Sections whose title produced no keyword, so their jobs fell back to a
/// URL slug or `ASSET`: where a keyword from the LLM helps.
pub fn sections_needing_keyword(parsed: &ParsedEmail) -> Vec<(String, String)> {
    parsed
        .sections
        .iter()
        .filter(|s| s.keyword.is_none() && !s.jobs.is_empty())
        .filter_map(|s| s.title.clone().map(|t| (s.index_str.clone(), t)))
        .collect()
}

/// Give a section a keyword. With `all_jobs` false only jobs that fell back
/// to `ASSET` change; attachment jobs keep their file-name keyword.
pub fn set_section_keyword(parsed: &mut ParsedEmail, index: &str, keyword: &str, all_jobs: bool) -> bool {
    let Some(s) = parsed.sections.iter_mut().find(|s| s.index_str == index) else {
        return false;
    };
    let mut changed = false;
    for j in &mut s.jobs {
        if j.tier != Tier::Attachment && (all_jobs || j.keyword == "ASSET") {
            j.keyword = keyword.to_string();
            changed = true;
        }
    }
    if changed {
        s.keyword = Some(keyword.to_string());
    }
    changed
}