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
use std::collections::{HashMap, HashSet};
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
    /// A transfer service's delivery names the sender, and the name is a
    /// roster surname or full name (P15.8).
    LockerSender,
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
    /// "2 ΠΡΩΤΑ ΒΙΝΤΕΟ": only the first N videos of the article
    /// (plan P4.33). `None` is all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_videos: Option<u32>,
    /// Where `keyword` came from (P4.34): what [`keyword_verdicts`] judges
    /// it by. Not serialised: decided and used between parse and enqueue.
    #[serde(skip)]
    pub keyword_from: KeywordSource,
}

/// Where a job's keyword came from, best first (P4.34).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeywordSource {
    /// A line written for this link alone ("Διαγωνισμός ύπνου" above it).
    Caption,
    /// The numbered section's title.
    Title,
    /// The LLM, from the link's own title and the mail (validated).
    Llm,
    /// The video's or page's own title (oEmbed, `<title>`), without the LLM.
    LinkTitle,
    /// An attachment's file name.
    FileName,
    /// The subject of an unnumbered mail: one name for every link in it.
    Subject,
    /// The first text line of an unnumbered mail.
    Preamble,
    /// The link's own slug.
    Url,
    /// Nothing usable: `ASSET`.
    #[default]
    Fallback,
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
    /// A transfer service's mail about files we sent, or other mail of the
    /// service (P15.1): nothing in it is queued.
    pub const LOCKER_NOTICE: &str = "LOCKER_NOTICE";
    /// [`LOCKER_NOTICE`]'s detail when the mail was the service's but no
    /// template recognised it, and it holds a link to files: MCR checks it.
    pub const LOCKER_NOTICE_FILE_LINK: &str = "UNRECOGNISED_WITH_FILE_LINK";
    /// A mail from outside the newsroom linked to sites the parser does not
    /// know; those links were not queued (P15.2). MCR may queue them by hand.
    pub const OUTSIDER_UNKNOWN_SITES: &str = "OUTSIDER_UNKNOWN_SITES";
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
    /// Keywords made from the videos' own titles (P4.34); detail
    /// `1A=DUNEPART, 1B=…`.
    pub const KEYWORD_FROM_TITLES: &str = "KEYWORD_FROM_TITLES";
    /// Keywords the meter still scores low after every source was tried
    /// (P4.34); detail `1A TREILER (generic, shared by 4 links), …`.
    pub const KEYWORD_UNCERTAIN: &str = "KEYWORD_UNCERTAIN";
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

/// Each word of `s` with the word after it, when only spaces stand between
/// them: «ΜΑΡΙΑ - ΚΑΙΡΟΣ» is two things, «ΜΑΡΙΑ ΚΑΙΡΟΣ» may be one name (P15.8).
fn tokens_with_next(s: &str) -> Vec<(&str, Option<&str>)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(b)) => {
                spans.push((b, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(b) = start {
        spans.push((b, s.len()));
    }
    spans
        .iter()
        .enumerate()
        .map(|(i, &(b, e))| {
            let next = spans.get(i + 1).filter(|&&(nb, _)| s[e..nb].chars().all(char::is_whitespace)).map(|&(nb, ne)| &s[nb..ne]);
            (&s[b..e], next)
        })
        .collect()
}

enum Lookup {
    One(String),
    /// Several people answer to it (sorted surnames).
    Ambiguous(Vec<String>),
    None,
}

fn pick(mut people: Vec<String>) -> Lookup {
    people.sort();
    people.dedup();
    match people.len() {
        0 => Lookup::None,
        1 => Lookup::One(people.remove(0)),
        _ => Lookup::Ambiguous(people),
    }
}

/// Endings of Greek surnames (and of the Slavic ones guests bring), in
/// ELOT 743 Latin: ΚΟΥΝΑΛΑΚΗΣ, ΣΚΟΥΛΑΚΗ, ΘΕΟΦΙΛΟΥ, ΠΕΤΡΩΦ.
const SURNAME_ENDINGS: &[&str] =
    &["IS", "AS", "OS", "ES", "OU", "AKI", "IDI", "IDOU", "OPOULOU", "OF", "OV", "EV", "AKIS"];

/// A word that could be a surname after a first name: four letters or more,
/// not a word the subject uses for something else, and a surname's ending
/// («ΛΑΙΒ», «ΧΑΝΙΑ», «ΤΕΛΕΥΤΑΙΟ» have none) or a surname on the roster
/// (`on_roster`) (P15.8).
fn looks_like_surname(word: &str, on_roster: impl Fn(&str) -> bool) -> bool {
    let k = latin_key(word);
    word.chars().all(char::is_alphabetic)
        && k.len() >= 4
        && ![STOPWORDS, NOT_A_NAME, ARTICLES, TOPIC_WORDS].iter().any(|list| list.contains(&k.as_str()))
        && !is_generic_keyword(&k)
        && (SURNAME_ENDINGS.iter().any(|e| k.ends_with(e)) || on_roster(&k))
}

/// Whether `a` becomes `b` with at most one letter added, dropped or
/// changed: «ΚΟΚΙΝΑΚΗ» for ΚΟΚΚΙΝΑΚΗ.
fn one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let same_start = short.iter().zip(long.iter()).take_while(|(x, y)| x == y).count();
    let skip = if short.len() == long.len() { 1 } else { 0 };
    short[(same_start + skip).min(short.len())..] == long[(same_start + 1).min(long.len())..]
}

/// Whether `word` is surname `key` as written in a subject: the same, its
/// genitive / nominative form, or either with one letter wrong (P15.8).
fn same_surname(key: &str, word: &str) -> bool {
    let bare = |s: &str| s.strip_suffix('S').unwrap_or(s).to_string();
    key == word
        || inflected(key, word)
        || (word.len() >= 4 && [(key.to_string(), word.to_string()), (bare(key), bare(word))].iter().any(|(k, w)| one_edit(k, w)))
}

/// Genitive / nominative forms of one name: ΑΝΝΑ ↔ ΑΝΝΑΣ, ΠΑΠΑΔΑΚΗΣ ↔ ΠΑΠΑΔΑΚΗ.
fn inflected(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 4 && long.len() - short.len() <= 2 && long.starts_with(short)
}

/// What a roster key is of its journalist.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    /// The surname, or the word of the full name that is the surname.
    Surname,
    /// An alias that is not one of the first names.
    Alias,
    /// A word of the full name other than the surname, or a one-word alias
    /// equal to one (P15.8): «ΓΙΑΝΝΗΣ» alone names whoever is called that.
    FirstName,
}

struct Key {
    key: String,
    surname: String,
    kind: KeyKind,
}

struct Roster<'a> {
    keys: Vec<Key>,
    journalists: &'a [Journalist],
}

impl<'a> Roster<'a> {
    fn new(journalists: &'a [Journalist]) -> Self {
        let mut keys = Vec::new();
        for j in journalists {
            let surname = latin_key(&j.surname);
            let mut add = |k: String, kind: KeyKind| {
                if !k.is_empty() {
                    keys.push(Key { key: k, surname: j.surname.clone(), kind });
                }
            };
            add(surname.clone(), KeyKind::Surname);
            // First names from the full name. Two staff sharing a first name
            // is handled by the ambiguity rule, not by leaving them out.
            let mut first_names = Vec::new();
            for t in tokens(&j.full_name) {
                let k = latin_key(t);
                if k == surname || inflected(&k, &surname) {
                    add(k, KeyKind::Surname);
                } else {
                    first_names.push(k.clone());
                    add(k, KeyKind::FirstName);
                }
            }
            for a in &j.aliases {
                let k = latin_key(a);
                let one_word = tokens(a).count() == 1;
                // «ΚΩΣΤΑ» for "Kostas" is the first name too.
                let first = first_names.iter().any(|f| *f == k || inflected(f, &k));
                let kind = if one_word && first { KeyKind::FirstName } else { KeyKind::Alias };
                add(k, kind);
            }
        }
        Self { keys, journalists }
    }

    /// The people `token` names, each with whether it names them only by a
    /// first name: the exact keys, else the genitive / nominative forms.
    fn candidates(&self, token: &str) -> Vec<(String, bool)> {
        let tok = latin_key(token);
        if tok.len() < 3 {
            return Vec::new();
        }
        let collect = |matches: &dyn Fn(&Key) -> bool| {
            let mut found: Vec<(String, bool)> = Vec::new();
            for k in self.keys.iter().filter(|k| matches(k)) {
                let first = k.kind == KeyKind::FirstName;
                match found.iter_mut().find(|(s, _)| *s == k.surname) {
                    Some((_, only_first)) => *only_first &= first,
                    None => found.push((k.surname.clone(), first)),
                }
            }
            found.sort();
            found
        };
        let exact = collect(&|k| k.key == tok);
        if !exact.is_empty() {
            return exact;
        }
        collect(&|k| inflected(&k.key, &tok))
    }

    /// Whether `token` is a name of `surname`'s journalist of this kind:
    /// the surname (`want_surname`), or another of their names.
    fn names(&self, token: &str, surname: &str, want_surname: bool) -> bool {
        let k = latin_key(token);
        k.len() >= 3
            && self
                .keys
                .iter()
                .filter(|key| key.surname == surname && (key.kind == KeyKind::Surname) == want_surname)
                .any(|key| key.key == k || inflected(&key.key, &k))
    }

    fn lookup(&self, token: &str) -> Lookup {
        pick(self.candidates(token).into_iter().map(|(s, _)| s).collect())
    }

    /// [`Self::lookup`] of a subject word, given the word after it (P15.8).
    /// A first name followed by someone else's surname («ΓΙΑΝΝΗΣ ΚΟΥΝΑΛΑΚΗΣ»)
    /// is not the person whose first name it is. `next` is the word after
    /// it only when nothing but spaces stands between them.
    fn lookup_before(&self, token: &str, next: Option<&str>) -> Lookup {
        let on_roster = |w: &str| self.keys.iter().any(|k| k.kind == KeyKind::Surname && k.key == w);
        let next = next.filter(|n| looks_like_surname(n, on_roster)).map(latin_key);
        // Their own name after it, even misspelt by a letter («ΦΩΤΕΙΝΗ ΚΟΚΙΝΑΚΗ»).
        let theirs = |surname: &str, n: &str| {
            self.keys.iter().filter(|k| k.surname == surname).any(|k| match k.kind {
                KeyKind::Surname => same_surname(&k.key, n),
                _ => k.key == n || inflected(&k.key, n),
            })
        };
        let people = self
            .candidates(token)
            .into_iter()
            .filter(|(surname, only_first)| match &next {
                Some(n) if *only_first => theirs(surname, n),
                _ => true,
            })
            .map(|(s, _)| s)
            .collect();
        pick(people)
    }

    /// The one person a display name a transfer service shows («Nikos
    /// Example has sent you…») is, by surname written exactly (the name is
    /// nominative: «Manolis» is not the surname ΜΑΝΩΛΗ): a first name alone
    /// is never enough (P15.8). Two people of that surname: the one whose
    /// full name it is. A first name on the roster that is not theirs
    /// («Giannis Vardakis» for Nikos Vardakis) is someone else.
    fn by_full_or_surname(&self, name: &str) -> Option<String> {
        let words: Vec<String> = tokens(name).map(latin_key).filter(|k| k.len() >= 3).collect();
        let mut found: Vec<String> = self
            .keys
            .iter()
            .filter(|k| k.kind == KeyKind::Surname && words.contains(&k.key))
            .map(|k| k.surname.clone())
            .collect();
        found.sort();
        found.dedup();
        // ANTONIOS for ΑΝΤΩΝΗΣ, ΓΙΩΡΓΟ for ΓΙΩΡΓΟΣ.
        let near = |a: &str, b: &str| a == b || inflected(a, b) || (a.len() >= 4 && b.len() >= 4 && one_edit(a, b));
        let first_names: Vec<&Key> = self.keys.iter().filter(|k| k.kind == KeyKind::FirstName).collect();
        found.retain(|s| {
            let own_surname = |w: &str| self.keys.iter().any(|k| k.surname == *s && k.kind == KeyKind::Surname && k.key == w);
            let own_first = |w: &str| first_names.iter().any(|k| k.surname == *s && near(&k.key, w));
            let someone_elses = |w: &str| first_names.iter().any(|k| near(&k.key, w)) && !own_first(w);
            !words.iter().any(|w| !own_surname(w) && someone_elses(w))
        });
        if found.len() > 1 {
            found.retain(|s| {
                self.journalists
                    .iter()
                    .filter(|j| j.surname == *s)
                    .any(|j| tokens(&j.full_name).map(latin_key).all(|t| words.contains(&t)))
            });
        }
        (found.len() == 1).then(|| found.remove(0))
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

/// Who a transfer service's delivery says sent the files (P15.8).
struct DeliverySender {
    /// The address the notice names (`NoticeKind::Delivery`'s sender).
    address: Option<String>,
    /// The display name before «has sent you» / «σας έστειλε».
    name: Option<String>,
}

fn resolve_journalist(
    mail: &InboundMail,
    body: &str,
    roster: &Roster,
    locker_sender: Option<&DeliverySender>,
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
        let words = tokens_with_next(&c[2]);
        // The sender, when the subject's name could be them: a mail is rarely
        // for a namesake of the one who sent it (P15.8).
        let sender = roster.by_sender(&mail.from_address).filter(|s| s != "MCR");
        for (t, next) in words {
            match roster.lookup_before(t, next) {
                Lookup::One(s) => {
                    return ResolvedJournalist {
                        surname: s,
                        how: Resolution::Subject,
                    }
                }
                Lookup::Ambiguous(all) if sender.as_ref().is_some_and(|s| all.contains(s)) => {
                    return ResolvedJournalist {
                        surname: sender.clone().unwrap_or_default(),
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

    // b2. A transfer service's delivery names who sent the files: that
    // person (P15.8). By the address it names, which is exact; else by the
    // display name («Nikos Example has sent you…»), by surname.
    let by_locker = locker_sender.and_then(|d| {
        let by_address = d.address.as_deref().and_then(|a| roster.by_sender(a)).filter(|s| s != "MCR");
        by_address.or_else(|| d.name.as_deref().and_then(|n| roster.by_full_or_surname(n)).filter(|s| s != "MCR"))
    });
    if let Some(s) = by_locker {
        return ResolvedJournalist {
            surname: s,
            how: Resolution::LockerSender,
        };
    }

    // c. Sender address. The MCR desk is never the journalist: it passes on
    // what a journalist sent it (owner, 2026-10-05).
    if let Some(s)= roster.by_sender(&mail.from_address).filter(|s| s != "MCR") {
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
    let lines: Vec<&str> = body.lines().collect();
    let roles = line_roles(&lines, forward);
    lines
        .iter()
        .zip(roles)
        .filter(|(_, role)| *role == LineRole::Read)
        .map(|(line, _)| *line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a line of a mail is to the parser (plan P7.7): only `Read` lines
/// are parsed. The MCR mail view shows the others too, folded or dimmed,
/// so an operator can see what the parser left out and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineRole {
    Read,
    /// The header lines and markers of a forwarded message.
    Forwarded,
    /// Quoted history: `>` lines, and everything below a reply's header.
    Quoted,
    /// Everything below the `--` signature delimiter.
    Signature,
}

/// The role of each of `lines`; see [`strip_quotes_and_signature`].
fn line_roles(lines: &[&str], forward: bool) -> Vec<LineRole> {
    let mut forward = forward;
    let mut roles = vec![LineRole::Read; lines.len()];
    let mut read: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();

        if t == "--" {
            // signature delimiter ("-- ", often trimmed)
            roles[i..].fill(LineRole::Signature);
            break;
        }
        if t.starts_with('>') {
            roles[i] = LineRole::Quoted;
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
            if RE_FORWARD_MARKER.is_match(t) || !has_media_link(&read) {
                forward = true;
                roles[i] = LineRole::Forwarded;
                i += 1;
                continue;
            }
            roles[i..].fill(LineRole::Quoted);
            break;
        }
        if forward && (is_block_marker || RE_HEADER_ANY.is_match(t)) {
            roles[i] = LineRole::Forwarded;
            i += 1;
            continue;
        }
        read.push(line);
        i += 1;
    }
    roles
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
        r#"|wetransfer\.com|we\.tl|transfernow\.net|myairbridge\.com|filemail\.com|fex\.net|kdrive\.infomaniak\.com|1drv\.ms"#,
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
    // "ΓΙΑ ΣΗΜΕΡΙΝΟ ΚΑΛΕΣΜΕΝΟ": a day or a role, not who the mail is for.
    "SIMERINO", "SIMERINI", "SIMERINA", "SIMERINOU", "AVRIANO", "AVRIANI", "AVRIANA", "KALESMENO", "KALESMENI",
    "KALESMENOUS",
];

/// Topics that follow a first name in a subject («ΓΙΑ ΓΙΑΝΝΗ ΣΕΙΣΜΟΣ») and
/// end like a surname, in Latin (P15.8). Only [`looks_like_surname`] reads
/// them: after «ΓΙΑ» alone they are still the subject's keyword.
const TOPIC_WORDS: &[&str] = &[
    "KAIROS", "AGIOS", "AGIA", "AGIOU", "AGIAS", "SEISMOS", "SEISMOU", "EKLOGES", "AGROTES", "AGROTON", "DIMOS",
    "DIMOU", "PLIMMYRES", "PLIMMYRA", "ASTYNOMIA", "ASTYNOMIAS", "FOTIA", "FOTIES", "PYRKAGIA", "PYRKAGIES",
    "PYROSVESTIKI", "PYROSVESTIKIS", "NOSOKOMEIO", "NOSOKOMEIOU", "TROCHAIO", "KAKOKAIRIA", "KYVERNISI", "VOULI",
    "LIMANI", "AERODROMIO", "SCHOLEIO", "PANEPISTIMIO", "MATS", "AGONAS", "AGONES",
];

/// Greek articles and prepositions-with-article, in ELOT 743 Latin: after
/// "ΓΙΑ" they introduce a topic, not a person.
const ARTICLES: &[&str] = &["TO", "TA", "TI", "TIN", "TIS", "TON", "TOUS", "TOU", "THN", "TH", "O", "I", "OI", "ENA", "MIA", "ENAN"];

/// "2 ΠΡΩΤΑ ΒΙΝΤΕΟ", "ΤΑ ΔΥΟ ΠΡΩΤΑ", "τα 3 πρώτα βίντεο", "first 2 videos"
/// (in ELOT 743 Latin). The number word or digit before ΠΡΩΤΑ, and what
/// follows it: a video word, or nothing.
static RE_FIRST_N: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(\d{1,2}|ENA|MIA|DYO|DIO|TRIA|TESSERA|PENTE|EXI|EKSI)\s+PROT(?:A|ES|OUS)\b\s*(VINTEO|VIDEOS?|PLANA|APOSPASMATA|KLIP)?(.*)$",
    )
    .unwrap()
});
static RE_PLANA_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bGIA\s+PLANA\b").unwrap());
static RE_FIRST_N_EN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bFIRST\s+(\d{1,2}|ONE|TWO|THREE|FOUR|FIVE|SIX)\s+(?:VIDEOS?|CLIPS?)\b").unwrap()
});
/// "ΜΟΝΟ ΤΟ ΠΡΩΤΟ (ΒΙΝΤΕΟ)", "only the first video": one. "ΜΟΝΟ" is required
/// in Greek, so a sentence that merely mentions "το πρώτο βίντεο" is not
/// read as an instruction.
static RE_FIRST_ONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bMONO\s+TO\s+PROTO\b|\bONLY\s+THE\s+FIRST\s+(?:VIDEO|CLIP)\b").unwrap()
});

/// How many of a page's videos a line asks for, if it says.
pub fn first_n_videos(line: &str) -> Option<u32> {
    let latin = translit(line);
    if RE_FIRST_ONE.is_match(&latin) {
        return Some(1);
    }
    // "Τα 2 πρώτα γκολ" in a story title is not an instruction: the count
    // is read only before a video word, at the end of the line, or on a
    // "ΓΙΑ ΠΛΑΝΑ:" line.
    let greek = RE_FIRST_N.captures(&latin).filter(|c| {
        c.get(2).is_some()
            || !c[3].chars().any(|ch| ch.is_alphanumeric())
            || RE_PLANA_MARKER.is_match(&latin)
    });
    let word = greek
        .or_else(|| RE_FIRST_N_EN.captures(&latin))
        .map(|c| c[1].to_uppercase())?;
    let n = match word.as_str() {
        "ENA" | "MIA" | "ONE" => 1,
        "DYO" | "DIO" | "TWO" => 2,
        "TRIA" | "THREE" => 3,
        "TESSERA" | "FOUR" => 4,
        "PENTE" | "FIVE" => 5,
        "EXI" | "EKSI" | "SIX" => 6,
        digits => digits.parse().ok()?,
    };
    (1..=20).contains(&n).then_some(n)
}

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
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        if let Some(prev) = out.last_mut() {
            if continues_wrapped_url(prev, line) {
                let joined = format!("{}{}", prev.trim_end(), line.trim());
                *prev = joined;
                continue;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// Whether `next` is the rest of a link that `prev` ends with, broken by a
/// client's line wrap; see [`rejoin_wrapped_urls`].
fn continues_wrapped_url(prev: &str, next: &str) -> bool {
    const WRAP_WIDTH: usize = 70;
    let is_url_char = |c: char| c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c);
    let next = next.trim();
    let prev_trim = prev.trim_end();
    let ends_in_url = find_urls(prev_trim).last().is_some_and(|(r, _)| r.end == prev_trim.len());
    let continues = !next.is_empty()
        && next.chars().all(is_url_char)
        && !next.to_ascii_lowercase().starts_with("http")
        && !next.to_ascii_lowercase().starts_with("www.")
        && !starts_with_a_link(next)
        && !next.starts_with(|c: char| c.is_ascii_digit() && next.len() <= 4);
    let at_separator = prev_trim.ends_with(['&', '=', '?', '/', '-', '_', '%', '.'])
        || next.starts_with(['&', '=', '?', '/', '#', '%']);
    let wrap_long = prev.chars().count() >= WRAP_WIDTH;
    ends_in_url && continues && (at_separator || wrap_long)
}

/// A host name with a path at the start of a line: `clips.news.example/v/…`.
/// A file name (`story.html/amp`) is not a host.
static RE_HOST_WITH_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+([a-z]{2,24})/[^\s/]").unwrap()
});
const FILE_EXTENSIONS: &[&str] = &["html", "htm", "shtml", "php", "asp", "aspx", "jsp", "cfm", "cgi"];

/// Whether `line` starts with a link of its own (P15.9): a bare link on a
/// known host, or a host name with a path. Such a line is never the rest of
/// the link above it.
fn starts_with_a_link(line: &str) -> bool {
    if RE_BARE_URL.find(line).is_some_and(|m| m.start() == 0) {
        return true;
    }
    RE_HOST_WITH_PATH.captures(line).is_some_and(|c| !FILE_EXTENSIONS.contains(&c[1].to_ascii_lowercase().as_str()))
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

/// Sentence punctuation, Greek included (ano teleia in both code points,
/// Greek question mark), smart quotes, an ellipsis, markdown bold, and a
/// separator left by split_glued.
const TRAILING: &[char] = &[
    '.', ',', ';', ':', '!', '?', '>', '»', '"', '\'', '…', '\u{0387}', '\u{00B7}', '\u{037E}', '”', '’', '“', '‘',
    '*', '_', '|',
];

/// What a sender glues onto the end of a link to say what it holds
/// ("…/arthro/-ΒΙΝΤΕΟ", "watch?v=…-BINTEO", "…-ΦΩΤΟ"), in capitals.
/// Greek capitals never end a real link (sites write slugs in lowercase
/// Latin, or percent-encoded), so these go whether or not a separator
/// precedes them. The Latin ones are the same words typed on a Latin
/// keyboard ("BINTEO" is ΒΙΝΤΕΟ letter for letter), and are taken only
/// after a `-`, `_` or `/`. Not "VIDEO" or "PHOTO": real paths end in those.
const GREEK_ANNOTATIONS: &[&str] = &[
    "ΒΙΝΤΕΟ", "ΒΊΝΤΕΟ", "ΒΙΝΤΕΑ", "ΦΩΤΟ", "ΦΩΤΟΓΡΑΦΙΑ", "ΦΩΤΟΓΡΑΦΙΕΣ", "ΕΙΚΟΝΑ", "ΕΙΚΟΝΕΣ", "ΠΛΑΝΑ",
    "ΑΠΟΣΠΑΣΜΑ", "ΔΗΛΩΣΕΙΣ", "ΗΧΟΣ",
];
const LATIN_ANNOTATIONS: &[&str] = &["BINTEO", "VINTEO", "FOTO", "EIKONES", "PLANA"];

/// A word, or its percent-encoded form (a link copied from a browser's
/// address bar): `%CE%92%CE%99…` in either hex case.
fn annotation_forms(word: &str) -> [String; 3] {
    let upper: String = word.bytes().map(|b| format!("%{b:02X}")).collect();
    [word.to_string(), upper.clone(), upper.to_lowercase()]
}

static RE_TRAILING_ANNOTATION: LazyLock<Regex> = LazyLock::new(|| {
    let alt = |words: &[&str]| -> String {
        let mut forms: Vec<String> = words.iter().flat_map(|w| annotation_forms(w)).map(|f| regex::escape(&f)).collect();
        forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
        forms.join("|")
    };
    let greek = alt(GREEK_ANNOTATIONS);
    let any = format!("{greek}|{}", alt(LATIN_ANNOTATIONS));
    // One word or several ("-ΒΙΝΤΕΟ+ΦΩΤΟ"), at the very end.
    Regex::new(&format!(
        r"(?:(?:[-_/]|%20)+(?:{any})|(?:{greek}))(?:(?:[-_/+&]|%20)+(?:{any}))*$"
    ))
    .unwrap()
});

/// The annotation a sender glued to the end of `url`, if any (see
/// [`GREEK_ANNOTATIONS`]): its byte offset, so the link ends before it.
pub fn trailing_annotation(url: &str) -> Option<usize> {
    let m = RE_TRAILING_ANNOTATION.find(url)?;
    // "…/arthro/-ΒΙΝΤΕΟ": the slash is the article's own (sites answer for
    // "…/arthro/", and may not for "…/arthro").
    let cut = if url[m.start()..].starts_with('/') { m.start() + 1 } else { m.start() };
    // Something must be left that is still a link with a path or a query.
    let parsed = Url::parse(&url[..cut]).ok()?;
    (parsed.host_str().is_some() && (parsed.path().len() > 1 || parsed.query().is_some())).then_some(cut)
}

/// `url` without an annotation glued to its end ("…/arthro/-ΒΙΝΤΕΟ" →
/// "…/arthro/"); unchanged when it has none. For links recorded before the
/// parser cut these off (P3.9): their jobs still carry the word.
pub fn without_annotation(url: &str) -> &str {
    match trailing_annotation(url) {
        Some(cut) => url[..cut].trim_end_matches(TRAILING),
        None => url,
    }
}

/// Where the link is inside `raw`, a match of the URL patterns: without the
/// markdown emphasis before it, the punctuation a sentence glued after it,
/// and a "-ΒΙΝΤΕΟ" the sender glued on (P3.9). The mail view marks exactly
/// this much of the text, so the annotation shows as the sender's words.
fn link_bounds(raw: &str) -> std::ops::Range<usize> {
    let start = raw.len() - raw.trim_start_matches(['*', '_']).len();
    let mut u = raw[start..].trim_end_matches(TRAILING);
    // A closing parenthesis belongs to the URL only if it opened one.
    while u.ends_with(')') && u.matches(')').count() > u.matches('(').count() {
        u = u[..u.len() - 1].trim_end_matches(TRAILING);
    }
    let with_scheme = if u.to_ascii_lowercase().starts_with("www.") { format!("https://{u}") } else { u.to_string() };
    if let Some(cut) = trailing_annotation(&with_scheme) {
        let cut = cut - (with_scheme.len() - u.len());
        u = u[..cut].trim_end_matches(TRAILING);
    }
    // P15.9: a dash that is a whole last path segment ("…/arthro/-") is the
    // sender's, not the site's. A dash at the end of a slug ("…-me-barka-")
    // stays: news sites that cut long titles keep it in the real address.
    // Not in a query or a fragment either.
    if u.ends_with("/-") && !u.contains(['?', '#']) {
        u = &u[..u.len() - 1];
    }
    start..start + u.len()
}

/// Trim punctuation a sentence glued to the link, add the scheme to a bare
/// `www.` link, and unwrap redirect wrappers so the real target is queued.
fn clean_url(raw: &str) -> Option<String> {
    let mut u = raw[link_bounds(raw)].to_string();
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

/// Path segments of a link that does something to an account or a
/// subscription rather than show something (P15.1).
const ACTION_SEGMENTS: &[&str] = &[
    "unsubscribe",
    "unsub",
    "optout",
    "opt-out",
    "opt_out",
    "delete",
    "visitor-delete",
    "abuse",
    "preferences",
    "email-preferences",
    "manage-preferences",
];
const ACTION_QUERY_KEYS: &[&str] = &["delete-signature", "unsubscribe", "unsub", "optout"];
const ACTION_HOSTS: &[&str] = &["myaccount.google.com"];

/// Whether `url` is an action link (P15.1): unsubscribe, delete, report
/// abuse, mail preferences, a Google account page. Opening one does
/// something, so it is never a job, whatever its site. A sign-in page is not
/// one: opening it changes nothing, and it may carry the story onward
/// (`login/?next=…`). Whole path segments
/// only (`/news/world-delete-12345` is a story), percent-decoded, without a
/// page extension (`unsubscribe.php`); a query key such as
/// `delete-signature`; `category=abuse`; Google's account hosts.
pub fn is_action_link(url: &str) -> bool {
    let Ok(u) = Url::parse(url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    if ACTION_HOSTS.contains(&host.as_str()) {
        return true;
    }
    let action_segment = u.path_segments().into_iter().flatten().any(|s| {
        let s = percent_decoded(s).to_lowercase();
        let s = [".html", ".htm", ".php", ".aspx", ".asp", ".jsp"]
            .iter()
            .find_map(|e| s.strip_suffix(e))
            .unwrap_or(s.as_str());
        ACTION_SEGMENTS.contains(&s)
    });
    action_segment
        || u.query_pairs().any(|(k, v)| {
            let k = k.to_lowercase();
            ACTION_QUERY_KEYS.contains(&k.as_str()) || (k == "category" && v.eq_ignore_ascii_case("abuse"))
        })
}

/// `%xx` escapes decoded; an invalid escape is kept as written.
fn percent_decoded(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Public mail providers: an address there says nothing about the newsroom,
/// so their domains are never the station's own (P15.2). Exact registrable
/// domains, and families under any suffix (`hotmail.co.uk`).
const PUBLIC_MAIL_DOMAINS: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "msn.com",
    "ymail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "otenet.gr",
    "windowslive.com",
];
const PUBLIC_MAIL_FAMILIES: &[&str] = &["hotmail", "outlook", "live", "yahoo"];

fn is_public_mail_domain(domain: &str) -> bool {
    let site = omni_core::psl::registrable(domain);
    PUBLIC_MAIL_DOMAINS.contains(&site.as_str())
        || site.split('.').next().is_some_and(|first| PUBLIC_MAIL_FAMILIES.contains(&first))
}

/// The lowercased domain of a mail address, if it has one.
fn mail_domain(address: &str) -> Option<String> {
    let a = address.trim().trim_start_matches('<').trim_end_matches('>').to_ascii_lowercase();
    a.rsplit_once('@').map(|(_, d)| d.trim_end_matches('.').to_string()).filter(|d| !d.is_empty())
}

/// Whether a mail from `from` comes from outside the newsroom (P15.2): the
/// address is on no roster entry, and its domain is neither one of the
/// roster's own domains nor under one. The own domains are those of the
/// roster's addresses, public mail providers left out; nothing is
/// configured. A roster that yields no own domain (a fresh installation, a
/// roster without addresses) makes nobody an outsider: unknown sites are
/// tried, as before.
pub fn is_outsider(from: &str, roster: &[Journalist]) -> bool {
    let addr = from.trim().trim_start_matches('<').trim_end_matches('>').to_ascii_lowercase();
    if roster.iter().flat_map(|j| j.emails.iter()).any(|e| e.trim().eq_ignore_ascii_case(&addr)) {
        return false;
    }
    let own = own_mail_domains(roster);
    if own.is_empty() {
        return false;
    }
    let Some(domain) = mail_domain(&addr) else { return true };
    !own.iter().any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}

/// The station's own mail domains: those of the roster's addresses, public
/// mail providers left out. Nothing is configured.
fn own_mail_domains(roster: &[Journalist]) -> Vec<String> {
    roster
        .iter()
        .flat_map(|j| j.emails.iter())
        .filter_map(|e| mail_domain(e))
        .filter(|d| !is_public_mail_domain(d))
        .collect()
}

/// Whether `url` is one of a transfer service's own pages rather than the
/// link to the files (P15.1): its front page, help, legal or contact pages,
/// mail images. Never a job.
pub fn is_transfer_service_page(url: &str) -> bool {
    omni_core::locker::is_transfer_service(url) && !omni_core::locker::is_share_link(url)
}

const LOCKER_DOMAINS: &[&str] = &[
    "wetransfer.com",
    "we.tl",
    "transfernow.net",
    "swisstransfer.com",
    "myairbridge.com",
    "mab.to",
    "filemail.com",
    "fex.net",
    "kdrive.infomaniak.com",
    "1drv.ms",
    "onedrive.live.com",
    "sharepoint.com",
    "drive.google.com",
    "dropbox.com",
];

const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".webp", ".heic"];

/// Servers that only serve photos, with addresses that carry no extension
/// (P15.5): `(host, path prefix)`. The host matches itself and its
/// subdomains. Two more are rules in [`is_image_server`]: Google's image
/// thumbnails (`encrypted-tbnN.gstatic.com`, and `*.gstatic.com/images`
/// with `q=tbn:…`), and an address whose `format=` or `fm=` names an image
/// type ([`IMAGE_FORMATS`]). Never a host that also serves video
/// (`video.twimg.com`, `*.googlevideo.com`, `*.fbcdn.net`).
const IMAGE_SERVERS: &[(&str, &str)] = &[
    ("images.openai.com", "/"),
    ("i.pinimg.com", "/"),
    ("pbs.twimg.com", "/media/"),
    ("lh3.googleusercontent.com", "/"),
    ("lh4.googleusercontent.com", "/"),
    ("lh5.googleusercontent.com", "/"),
    ("lh6.googleusercontent.com", "/"),
    ("braze-images.com", "/"),
];

/// The `format=` / `fm=` values that say the address is a photo.
const IMAGE_FORMATS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "avif"];

/// A Google Photos video stream on an `lhN.googleusercontent.com` host: its
/// address's options after the last `=` hold `dv` or `m<digits>` (P15.5),
/// alone or among others separated by `-` (`=w1280-h720-dv`, `=m22-no`,
/// P15.12b). Never a photo.
fn is_google_photos_video(host: &str, path: &str) -> bool {
    host.ends_with(".googleusercontent.com")
        && path.rsplit_once('=').is_some_and(|(_, tail)| {
            tail.split('-').any(|o| {
                o == "dv" || o.strip_prefix('m').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
        })
}

/// Whether the address's path ends in an image file's extension: a photo
/// for certain, unlike one matched only by its server ([`is_image_server`]).
pub fn has_image_extension(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| {
        let path = u.path().to_ascii_lowercase();
        IMAGE_EXTENSIONS.iter().any(|e| path.ends_with(e))
    })
}

/// A photo by its server or its query, though its path has no image extension (P15.5).
fn is_image_server(parsed: &Url, host: &str, path: &str) -> bool {
    if is_google_photos_video(host, path) {
        return false;
    }
    if IMAGE_SERVERS.iter().any(|(h, prefix)| host_matches(host, h) && path.starts_with(prefix)) {
        return true;
    }
    if host_matches(host, "gstatic.com") {
        let label = host.strip_suffix(".gstatic.com").unwrap_or("");
        let tbn = label.strip_prefix("encrypted-tbn").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        let thumbnail = path == "/images"
            && parsed.query_pairs().any(|(k, v)| k == "q" && v.to_ascii_lowercase().starts_with("tbn:"));
        if tbn || thumbnail {
            return true;
        }
    }
    parsed
        .query_pairs()
        .any(|(k, v)| (k == "format" || k == "fm") && IMAGE_FORMATS.contains(&v.to_ascii_lowercase().as_str()))
}

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
        ["vimeo.com", "dailymotion.com", "dai.ly", "streamable.com"].iter().any(|d| host_matches(&host, d))
    };
    if tier1 {
        return Tier::Tier1;
    }

    if has_image_extension(url) || is_image_server(&parsed, &host, &path) {
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

/// File types: what a file is, never what it shows (P15.10).
const FILE_TYPES: &[&str] = &[
    "MP4", "MOV", "MXF", "AVI", "MKV", "WMV", "M4V", "MTS", "JPG", "JPEG", "PNG", "HEIC", "ZIP", "RAR", "PDF", "DOCX",
    "WAV", "MP3",
];

/// The English words of a transfer service's notice («Nikos Example has
/// sent you…») that no Greek word becomes in ELOT 743 (no Greek letter
/// gives H, W or a C outside CH), the services' own names, and the fallback
/// keyword itself (P15.10). The notice words a Greek word can become (VIA is
/// ΒΙΑ, FILES ΦΙΛΕΣ, FILE ΦΙΛΕ, SAS ΣΑΣ) are only junk in a service's own
/// mail: see [`NOTICE_VOCABULARY_EN`].
const NOTICE_WORDS: &[&str] = &[
    "HAS", "HAVE", "SENT", "YOU", "DOWNLOADED", "RECEIVED", "TRANSFERNOW", "WETRANSFER", "SWISSTRANSFER", "MYAIRBRIDGE",
    "FILEMAIL", "DROPBOX", "ASSET",
];

/// English filler a notice or a newsletter is written in ("Here is the
/// video you might like"), past the English [`STOPWORDS`] (P15.10). Not a
/// word a news subject is about (NEWS, LIVE and MATCH stay).
const FILLER_WORDS: &[&str] = &[
    "YOUR", "THIS", "THAT", "BEEN", "MIGHT", "WILL", "ARE", "WAS", "WERE", "TOTAL", "ITEM", "ITEMS", "CLICK", "VIEW",
    "OPEN", "PLEASE", "DEAR", "HELLO", "HI", "THANKS",
];

/// What a transfer service's own mail is written in (P15.10): never a
/// keyword in mail from such a service, and still a topic in anyone else's
/// («ΜΕΤΑΦΟΡΑ ΑΣΘΕΝΟΥΣ»). Greek as written, the months in the genitive.
const NOTICE_VOCABULARY_EL: &[&str] = &[
    "ΣΑΣ", "ΕΣΤΕΙΛΕ", "ΜΕΣΩ", "ΕΚΑΝΕ", "ΜΕΤΑΦΟΡΤΩΣΗ", "ΛΗΨΗ", "ΛΗΓΕΙ", "ΜΕΤΑΦΟΡΑ", "ΜΕΤΑΦΟΡΑΣ", "ΗΜΕΡΕΣ", "ΣΤΑΛΘΕΙ", "ΕΧΟΥΝ",
    "ΕΝΤΟΠΙΣΕ", "ΕΓΚΥΡΗ", "ΑΡΧΕΙΟ", "ΑΡΧΕΙΑ", "ΚΩΔΙΚΟΣ", "ΕΠΙΒΕΒΑΙΩΣΗ", "ΙΑΝΟΥΑΡΙΟΥ", "ΦΕΒΡΟΥΑΡΙΟΥ", "ΜΑΡΤΙΟΥ",
    "ΑΠΡΙΛΙΟΥ", "ΜΑΪΟΥ", "ΙΟΥΝΙΟΥ", "ΙΟΥΛΙΟΥ", "ΑΥΓΟΥΣΤΟΥ", "ΣΕΠΤΕΜΒΡΙΟΥ", "ΟΚΤΩΒΡΙΟΥ", "ΝΟΕΜΒΡΙΟΥ", "ΔΕΚΕΜΒΡΙΟΥ",
];
const NOTICE_VOCABULARY_EN: &[&str] = &[
    "VIA", "FILES", "FILE", "EXPIRE", "EXPIRES", "EXPIRED", "SUCCESSFULLY", "CONFIRM", "CONFIRMATION", "CODE", "TRANSFER", "TRANSFERS", "SECURE",
    "LARGE", "STORAGE", "ACCOUNT", "RECIPIENT", "RECIPIENTS", "SHARED", "DETECTED", "INVALID", "DOWNLOAD", "UPLOAD",
    "STILL", "ABOUT", "NOT", "OUT", "JANUARY", "FEBRUARY", "MARCH", "APRIL", "MAY", "JUNE", "JULY", "AUGUST",
    "SEPTEMBER", "OCTOBER", "NOVEMBER", "DECEMBER",
];

/// [`NOTICE_VOCABULARY_EL`] and [`NOTICE_VOCABULARY_EN`] as keyword tokens.
fn notice_vocabulary() -> Vec<String> {
    NOTICE_VOCABULARY_EL.iter().map(|w| latin_key(w)).chain(NOTICE_VOCABULARY_EN.iter().map(|w| w.to_string())).collect()
}

/// A camera's or phone's file name: IMG4411, DSC01234, GOPR0042, FILE382,
/// WA0001 (WhatsApp), CLIP7.
static RE_CAMERA_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:IMG|DSC|DSCN|DSCF|MVI|VID|GOPR|GP|DJI|PXL|FILE|CLIP|ASSET|VIDEO|WA)\d+$").unwrap());

/// A date or a time written out (27/10, 27-10-2026, 2026-10-27, 14:30):
/// taken out of a text before its words are read (P15.10).
static RE_DATE_TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:\d{4}-\d{1,2}-\d{1,2}|\d{1,2}[/.\-]\d{1,2}(?:[/.\-]\d{2,4})?|\d{1,2}:\d{2}(?::\d{2})?)\b").unwrap()
});

/// `text` without mail addresses, dates and times: none of them names a story.
fn without_addresses_and_dates(text: &str) -> String {
    RE_DATE_TIME.replace_all(&RE_ADDRESS.replace_all(text, " "), " ").into_owned()
}

/// Whether a keyword token (Latin capitals and digits) names nothing
/// (P15.10): a file type, a camera's file name, a long bare number (a
/// phone, a file number, a date run together), a word of a transfer notice, or one
/// of `own`: the labels of the station's own mail domains (from the
/// roster) and, in a transfer service's own mail, its vocabulary.
fn is_junk_token(t: &str, own: &[String]) -> bool {
    // A long bare number: a phone, a file number, a date run together
    // (20261027). Short numbers and codes (112, 1866, F35, A320, 20I) stay.
    let long_number = t.len() >= 5 && t.chars().all(|c| c.is_ascii_digit());
    FILE_TYPES.contains(&t)
        || NOTICE_WORDS.contains(&t)
        || FILLER_WORDS.contains(&t)
        || RE_CAMERA_NAME.is_match(t)
        || long_number
        || own.iter().any(|l| l == t)
}

/// Whether a keyword is junk through and through: one junk token, or two
/// run together (`HASSENT`, `MP4FILE`). Used where a keyword arrives whole
/// (the LLM's), with no word boundaries left.
fn is_junk_keyword(k: &str) -> bool {
    is_junk_token(k, &[]) || (1..k.len()).any(|i| k.is_char_boundary(i) && is_junk_token(&k[..i], &[]) && is_junk_token(&k[i..], &[]))
}

/// The labels of the station's own mail domains (`example.gr` → EXAMPLE,
/// GR), from the roster's addresses as [`is_outsider`] finds them: never
/// part of a keyword (P15.10).
fn own_domain_labels(roster: &[Journalist]) -> Vec<String> {
    let mut labels: Vec<String> = own_mail_domains(roster)
        .iter()
        .flat_map(|d| d.split('.').map(|l| l.to_ascii_uppercase()).collect::<Vec<_>>())
        .map(|l| l.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>())
        .filter(|l| !l.is_empty())
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

/// `[A-Z0-9]{2,20}` from free text: transliterate, drop stopwords and short
/// words, keep the first two, join.
pub fn keyword_from_text(text: &str) -> Option<String> {
    keyword_from_text_with(text, &[])
}

/// [`keyword_from_text`], also leaving out the labels of the station's own
/// mail domains (`own`).
fn keyword_from_text_with(text: &str, own: &[String]) -> Option<String> {
    let text = RE_DATE_TIME.replace_all(text, " ");
    keyword_from_tokens(tokens(&translit(&text)).map(|t| t.to_string()).collect(), false, own)
}

/// "VINTEOOO" → "VINTEO": a letter typed three or more times in a row is
/// emphasis ("βιντεοοο"), never spelling. Greek and Latin words double a
/// letter at most.
fn fold_stretched(t: &str) -> String {
    let chars: Vec<char> = t.chars().collect();
    let mut out = String::with_capacity(t.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut j = i;
        while j < chars.len() && chars[j] == c {
            j += 1;
        }
        let run = j - i;
        let keep = if run >= 3 && c.is_alphabetic() { 1 } else { run };
        out.extend(std::iter::repeat(c).take(keep));
        i = j;
    }
    out
}

/// Words that, alone, do not name a story: what kind of material it is,
/// not what it is about (P4.34). "TREILER" ×4 named four different films.
const GENERIC: &[&str] = &[
    "VINTEO", "VINTEAKI", "VIDEO", "VIDEOS", "TREILER", "TRAILER", "TRAILERS", "TEASER", "PLANA", "PLANO", "YLIKO",
    "VIRAL", "THEMA", "THEMATA", "REPORTAZ", "NEWS", "LINK", "LINKS", "ASSET", "KLIP", "CLIP", "EIKONES", "FOTO",
    "APOSPASMA", "APOSPASMATA", "DILOSI", "DILOSEIS", "SYNENTEFXI", "OFFICIAL", "EPISIMO", "PROMO", "SPOT", "DELTIO",
    "TYPOU", "EPIKAIROTITA", "DIETHNI", "DIETHNES", "KALIMERA", "EFCHARISTO", "STOICHEIA", "EPIKOINONIAS",
    // A platform's own name: the title of its login wall ("Instagram",
    // "Log in • Instagram") or of a page it would not show, never the story.
    "INSTAGRAM", "FACEBOOK", "YOUTUBE", "TIKTOK", "TWITTER", "VIMEO", "DAILYMOTION", "THREADS", "LOGIN", "LOG",
    "SIGN", "WATCH", "POST", "REEL", "REELS", "SHORTS", "STATUS",
];

/// Whether `keyword` is only generic words (one, or two run together), the
/// kind that names nothing: TREILER, VINTEOOO, PLANAVINTEO.
pub fn is_generic_keyword(keyword: &str) -> bool {
    let k = fold_stretched(keyword);
    let generic = |s: &str| GENERIC.contains(&s) || STOPWORDS.contains(&s);
    generic(&k) || (1..k.len()).any(|i| k.is_char_boundary(i) && generic(&k[..i]) && generic(&k[i..]))
}

/// Words in `text` a keyword could be made from (not stopwords, 3+ chars).
fn meaningful_words(text: &str) -> usize {
    tokens(&translit(text))
        .map(|t| fold_stretched(&t.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()))
        .filter(|t| t.len() >= 3 && !STOPWORDS.contains(&t.as_str()))
        .count()
}

/// A keyword from a video's or page's own title ("Dune: Part Three |
/// Official Trailer" → DUNEPARTTHREE): generic words dropped first, so the
/// kind of clip does not crowd out its subject. `None` when only generic
/// words are left.
pub fn keyword_from_title(title: &str) -> Option<String> {
    let toks: Vec<String> = tokens(&translit(title))
        .map(|t| t.to_string())
        .filter(|t| !GENERIC.contains(&fold_stretched(t).as_str()))
        .collect();
    keyword_from_tokens(toks, false, &[]).filter(|k| !is_generic_keyword(k))
}

/// The first two words that name something, joined. Junk (P15.10) is
/// never one of them; with none left, `None`, and the next source is tried.
fn keyword_from_tokens(toks: Vec<String>, letters_only: bool, own: &[String]) -> Option<String> {
    let folded: Vec<String> = toks
        .into_iter()
        .map(|t| fold_stretched(&t.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_uppercase()))
        .collect();
    // "You Tube", "you-tube": one name written apart. Junk goes by whole
    // words, so without this YOU would go and TUBE stay.
    let mut joined_up: Vec<String> = Vec::with_capacity(folded.len());
    for t in folded {
        if t == "TUBE" && joined_up.last().is_some_and(|p| p == "YOU") {
            joined_up.pop();
            joined_up.push("YOUTUBE".into());
        } else {
            joined_up.push(t);
        }
    }
    // A camera's file name written apart ("IMG_4411", "FILE 382"): the
    // number after the word is the file's, not the story's.
    let camera_number = |i: usize| {
        i > 0 && joined_up[i].chars().all(|c| c.is_ascii_digit()) && RE_CAMERA_NAME.is_match(&format!("{}0", joined_up[i - 1]))
    };
    let kept: Vec<String> = (0..joined_up.len()).filter(|i| !camera_number(*i)).map(|i| joined_up[i].clone()).collect();
    let picked: Vec<String> = kept
        .into_iter()
        .filter(|t| t.len() >= 3)
        .filter(|t| !letters_only || t.chars().all(|c| c.is_ascii_alphabetic()))
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .filter(|t| !is_junk_token(t, own))
        .take(2)
        .collect();
    // Numbers alone ("820 2026") name nothing; beside a word they may (ΜΗΝΥΜΑ 112).
    if !picked.iter().any(|t| t.chars().any(|c| c.is_ascii_alphabetic())) {
        return None;
    }
    let joined: String = picked.concat().chars().take(MAX_KEYWORD).collect();
    (joined.len() >= 2).then_some(joined)
}

/// Keyword from a human-readable URL slug (`/eimaste-oloi-mia-oikogeneia`).
/// Opaque ids (`watch?v=dQw4w9`, `/reel/C8xYz`) have no separators and are
/// skipped, so they fall through to `ASSET` rather than become gibberish.
pub fn keyword_from_url(url: &str) -> Option<String> {
    keyword_from_url_with(url, &[])
}

/// [`keyword_from_url`], also leaving out the station's own domain labels.
fn keyword_from_url_with(url: &str, own: &[String]) -> Option<String> {
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
        if let Some(k) = keyword_from_tokens(toks, true, own) {
            return Some(k);
        }
    }
    None
}

fn keyword_from_filename(name: &str, own: &[String]) -> Option<String> {
    let stem = name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name);
    let toks = tokens(&translit(stem)).map(|t| t.to_string()).collect();
    keyword_from_tokens(toks, true, own)
}

/// Each link's own line, as a keyword: the text before it on its line, or
/// the line right above it when that line has no link of its own. Only a
/// line with one link (two links on a line share whatever it says), and
/// only a caption that names something (not "Καλημέρα", not "βίντεο").
fn link_captions(lines: &[Line], own: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        if line.urls.len() != 1 {
            continue;
        }
        let same_line = line.title_text();
        let above = (i > 0 && lines[i - 1].urls.is_empty() && !lines[i - 1].is_marker())
            .then(|| lines[i - 1].title_text())
            .flatten();
        let keyword = [same_line, above]
            .into_iter()
            .flatten()
            .filter_map(|t| keyword_from_text_with(&t, own))
            .find(|k| !is_generic_keyword(k));
        if let Some(k) = keyword {
            out.insert(line.urls[0].clone(), k);
        }
    }
    out
}

/// How sure the parser is that a job's keyword names its video (P4.34).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeywordVerdict {
    pub index_str: String,
    pub keyword: String,
    pub from: KeywordSource,
    /// 0.0–1.0. Below [`KEYWORD_SURE`] the keyword is worth a second look.
    pub score: f64,
    /// Why it is lower than its source alone would make it, in English
    /// (`generic`, `shared by 4 links`, `2 of 7 words`).
    pub reasons: Vec<String>,
}

impl KeywordVerdict {
    pub fn is_low(&self) -> bool {
        self.score < KEYWORD_SURE
    }
}

/// A keyword scored below this gets the link's own title and, when the LLM
/// is on, a proposal from it.
pub const KEYWORD_SURE: f64 = 0.6;

/// The keyword confidence meter (P4.34), one verdict per job.
///
/// * the source: a caption or numbered title names the story; a subject
///   names a whole mail; a URL slug or ASSET names nothing in particular;
/// * a generic keyword (TREILER, VINTEOOO, PLANA) names nothing whatever
///   its source;
/// * one subject or first line on several links ("ΤΡΕΪΛΕΡ" on four
///   films, the subject on nine clips) cannot tell them apart. Links under
///   one numbered title share it by design and are not marked down;
/// * a keyword that kept two words of a long subject may have kept the
///   wrong two ("ΣΤΟΙΧΕΙΑ ΕΠΙΚΟΙΝΩΝΙΑΣ" of a mail about a guest).
pub fn keyword_verdicts(parsed: &ParsedEmail) -> Vec<KeywordVerdict> {
    let mut uses: HashMap<&str, usize> = HashMap::new();
    for j in parsed.jobs() {
        *uses.entry(j.keyword.as_str()).or_default() += 1;
    }
    let mut out = Vec::new();
    for s in &parsed.sections {
        let source_words = s.title.as_deref().map(meaningful_words).unwrap_or(0);
        for j in &s.jobs {
            let mut score: f64 = match j.keyword_from {
                KeywordSource::Caption | KeywordSource::Title | KeywordSource::Llm => 0.9,
                KeywordSource::LinkTitle | KeywordSource::FileName => 0.8,
                KeywordSource::Subject => 0.7,
                KeywordSource::Preamble => 0.6,
                KeywordSource::Url => 0.5,
                KeywordSource::Fallback => 0.0,
            };
            let mut reasons = Vec::new();
            if is_generic_keyword(&j.keyword) {
                score = score.min(0.2);
                reasons.push("generic".to_string());
            }
            let shared = uses.get(j.keyword.as_str()).copied().unwrap_or(1);
            if shared > 1 && matches!(j.keyword_from, KeywordSource::Subject | KeywordSource::Preamble | KeywordSource::Url) {
                score -= 0.3;
                reasons.push(format!("shared by {shared} links"));
            }
            if matches!(j.keyword_from, KeywordSource::Subject | KeywordSource::Preamble | KeywordSource::Title) && source_words > 3 {
                score -= 0.15;
                reasons.push(format!("2 of {source_words} words"));
            }
            out.push(KeywordVerdict {
                index_str: j.index_str.clone(),
                keyword: j.keyword.clone(),
                from: j.keyword_from,
                score: round2(score.max(0.0)),
                reasons,
            });
        }
    }
    out
}

/// Give one job a better keyword (P4.34): from the LLM or the link's own
/// title. An attachment named by its file keeps that name.
pub fn set_job_keyword(parsed: &mut ParsedEmail, index: &str, keyword: &str, from: KeywordSource) -> bool {
    for s in &mut parsed.sections {
        for j in &mut s.jobs {
            if j.index_str == index && j.keyword_from != KeywordSource::FileName {
                j.keyword = keyword.to_string();
                j.keyword_from = from;
                return true;
            }
        }
    }
    false
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
fn subject_title(subject: &str, roster: &Roster, journalist: &str, own: &[String], sender_words: &[String]) -> Option<(String, String)> {
    let latin = subject_without_prefixes(&translit(&without_addresses_and_dates(subject)));
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
        // A transfer notice's sender, known to the roster or not (P15.10).
        .filter(|t| !sender_words.iter().any(|w| w == t || inflected(w, t)))
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
        keyword_from_text_with(&words.join(" "), own)
    };
    let keyword = keyword_from_text_with(&kept.join(" "), own).or_else(unlisted_recipient)?;
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

/// One line of a mail as the MCR mail view shows it (plan P7.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewLine {
    pub role: LineRole,
    pub text: String,
    /// Each link on the line: the bytes of `text` to mark, and the address
    /// the parser reads there (scheme added, redirect wrappers removed).
    pub links: Vec<(std::ops::Range<usize>, String)>,
}

/// A stored mail text (`InboundMail::readable_body`) line by line, with the
/// role the parser gives each line and every link it would find on it.
///
/// The same steps as [`cleaned_body`] up to the cut, so what the view calls
/// read is what the parser read; but nothing is dropped, and a link a
/// plain-text client wrapped over two lines is joined back into one line, as
/// the parser joins it.
pub fn view_lines(body: &str, subject: &str) -> Vec<ViewLine> {
    let body = body.replace(INVISIBLE, "");
    let body = RE_CID_PLACEHOLDER.replace_all(&body, "");
    let lines: Vec<&str> = body.lines().collect();
    let roles = line_roles(&lines, is_forward(subject));
    let mut out: Vec<ViewLine> = Vec::with_capacity(lines.len());
    for (line, role) in lines.iter().zip(roles) {
        if let Some(prev) = out.last_mut() {
            if prev.role == role && continues_wrapped_url(&prev.text, line) {
                prev.text = format!("{}{}", prev.text.trim_end(), line.trim());
                continue;
            }
        }
        out.push(ViewLine { role, text: line.to_string(), links: Vec::new() });
    }
    for line in &mut out {
        line.links = find_urls(&line.text)
            .into_iter()
            .map(|(r, url)| {
                let inner = link_bounds(&line.text[r.clone()]);
                (r.start + inner.start..r.start + inner.end, url)
            })
            .collect();
    }
    out
}

/// A link to a document (PDF, Office, calendar, contact card), not a page.
pub fn is_document_link(url: &str) -> bool {
    let path = Url::parse(url).map(|u| u.path().to_ascii_lowercase()).unwrap_or_default();
    DOCUMENT_EXTENSIONS.iter().any(|e| path.ends_with(e))
}

pub fn parse(mail: &InboundMail, roster: &[Journalist], cfg: &ParserConfig) -> ParsedEmail {
    let mut warnings = Vec::new();
    let roster_ix = Roster::new(roster);

    // 1. Cleanup.
    let body = cleaned_body(mail);

    // A transfer service's mail about files the desk sent, or its other mail
    // (P15.1): every link is seen and none is queued.
    let own: Vec<String> = roster.iter().filter(|j| j.surname == "MCR").flat_map(|j| j.emails.iter().cloned()).collect();
    let notice = crate::locker_notice::notice_kind(mail, &own);
    let quiet = notice.queues_nothing();

    // 2. Journalist.
    let locker_sender = match &notice {
        crate::locker_notice::NoticeKind::Delivery { sender } => {
            Some(DeliverySender { address: sender.clone(), name: crate::locker_notice::sender_name(mail) })
        }
        _ => None,
    };
    let journalist = resolve_journalist(mail, &body, &roster_ix, locker_sender.as_ref(), &mut warnings);
    let unresolved = journalist.how == Resolution::Unresolved;
    // A service mail no template recognised, yet with a link to files: still
    // no job, but MCR is told to look (P15.1).
    let mut unrecognised_file_link = false;
    // A mail from outside the newsroom (P15.2): a link to a site the parser
    // does not know is held for MCR instead of fetched. Staff mail keeps the
    // owner's rule that such a site is tried (2026-10-05).
    let outsider = is_outsider(&mail.from_address, roster);
    let mut held_unknown = false;

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
    // The station's own domain names are never a keyword (P15.10).
    let mut own_labels = own_domain_labels(roster);
    // Nor, in a transfer service's own mail, the words it is written in.
    if notice != crate::locker_notice::NoticeKind::NotANotice {
        own_labels.extend(notice_vocabulary());
    }
    // Nor are the names of who sent it (P15.10): the resolved journalist's
    // (surname, full name, aliases, in any case), and the sender a transfer
    // service's delivery names, whether or not the roster knows them.
    let sender_words: Vec<String> = locker_sender
        .as_ref()
        .and_then(|d| d.name.as_deref())
        .map(|n| tokens(n).map(latin_key).filter(|k| k.len() >= 3).collect())
        .unwrap_or_default();
    // The journalist by surname, or by first name right before it; a first
    // name or alias alone is left (ΝΙΚΗ, ΕΙΡΗΝΗ are words too).
    let named = journalist.surname != "MCR";
    let is_surname = |t: &str| named && roster_ix.names(t, &journalist.surname, true);
    let is_other_name = |t: &str| named && roster_ix.names(t, &journalist.surname, false);
    let is_sender = |t: &str| -> bool {
        let k = latin_key(t);
        sender_words.iter().any(|w| *w == k || inflected(w, &k))
    };
    // Mail addresses ("<nikos.home@mail.example>"), dates and times are no
    // words of the story either.
    let without_names = |text: &str| -> String {
        let latin = translit(&without_addresses_and_dates(text));
        let words: Vec<&str> = tokens(&latin).collect();
        words
            .iter()
            .enumerate()
            .filter(|(i, t)| {
                let full_name = is_other_name(t) && words.get(i + 1).is_some_and(|n| is_surname(n));
                !(is_sender(t) || is_surname(t) || full_name)
            })
            .map(|(_, t)| *t)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let keyword_of = |t: &str| keyword_from_text_with(&without_names(t), &own_labels);
    let with_keyword = |t: Option<String>| {
        let k = t.as_deref().and_then(keyword_of);
        (t, k)
    };
    let mut titles: Vec<(Option<String>, Option<String>, KeywordSource)> = raw
        .iter()
        .map(|s| {
            let (t, k) = with_keyword(section_title(s));
            (t, k, KeywordSource::Title)
        })
        .collect();

    if raw.is_empty() {
        let title = match subject_title(&mail.subject, &roster_ix, &journalist.surname, &own_labels, &sender_words) {
            Some((t, k)) => (Some(t), Some(k), KeywordSource::Subject),
            None => {
                // The first line before the links that names something (a
                // transfer notice's «X sent you 2 files» names nothing once
                // the sender and the notice's words are left out; the file's
                // name on the next line does).
                // Only down to the first real link: what follows it (a
                // footer, «Λήψη του Outlook για Android», a signature) is not
                // its name. A transfer service's own pages and action links
                // do not end the search (fixture 48 starts with one).
                let first_link = preamble
                    .iter()
                    .position(|l| l.urls.iter().any(|u| !is_transfer_service_page(u) && !is_action_link(u)));
                let upto = first_link.map_or(preamble.len(), |i| i + 1);
                let titled = |lines: &[Line]| -> Vec<String> {
                    lines.iter().filter(|l| !l.is_marker()).filter_map(|l| l.title_text()).collect()
                };
                let texts = titled(&preamble[..upto]);
                match texts.iter().find_map(|t| keyword_of(t).map(|k| (t.clone(), k))) {
                    Some((t, k)) => (Some(t), Some(k), KeywordSource::Preamble),
                    // Nothing above the first link names it (a link pasted
                    // first, a logo): the first line that reads as a title,
                    // as before this limit, wherever it is (P15.10 round 4).
                    None => {
                        let (t, k) = with_keyword(titled(&preamble).into_iter().next());
                        (t, k, KeywordSource::Preamble)
                    }
                }
            }
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
    let mut section_sources: Vec<KeywordSource> = Vec::new();
    let mut inherited_from = KeywordSource::Fallback;
    // One transfer sent under two links (MyAirBridge's short and alternate).
    let mut seen_shares: HashSet<String> = HashSet::new();

    for (s, (title, section_kw, kw_source)) in raw.iter().zip(titles) {
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
                // P15.1: never a job: anything in a notice, an action link
                // (unsubscribe, delete, report abuse, sign in), a transfer
                // service's own page, the same transfer a second time.
                // The transfer's identity is taken only by a link that passed
                // the other checks: a delete link first does not make the
                // download link a repeat.
                if quiet
                    || is_action_link(u)
                    || is_transfer_service_page(u)
                    || omni_core::locker::share_identity(u).is_some_and(|id| !seen_shares.insert(id))
                {
                    unrecognised_file_link |= notice == crate::locker_notice::NoticeKind::ServiceMail
                        && !is_action_link(u)
                        && omni_core::locker::is_share_link(u);
                    ignored_urls.push(u.clone());
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
        let mut held_in_section = false;
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
            // P15.2: an outsider's link to an unknown site that would have
            // been tried (marked, called video, story-like) waits for MCR.
            // Decided after the marker, so narrowing is as before.
            let hold = take && outsider && tier == Tier::Other;
            if hold {
                held_in_section = true;
                held_unknown = true;
            }
            if take && !hold {
                selected.push((u, tier, m));
            } else {
                ignored_urls.push(u);
            }
        }

        // A section whose links were held is not «without links».
        if selected.is_empty() && s.header.is_some() && !held_in_section {
            warnings.push(Warning::new(warnings::SECTION_WITHOUT_LINKS, Some(s.index.clone())));
        }

        // "ΓΙΑ ΠΛΑΝΑ: 2 ΠΡΩΤΑ ΒΙΝΤΕΟ" anywhere in the section: for the
        // article links in it (a platform post is one video already).
        let first_n = s.header.iter().chain(s.lines.iter()).find_map(|l| first_n_videos(&l.text));
        let many = selected.len() > 1;
        // Several links, each with its own line ("Διαγωνισμός ύπνου" above
        // one, "ΔΙΑΓΩΝΙΣΜΟΣ ΠΑΡΚΑΡΙΣΜΑΤΟΣ" above the next) in a mail whose
        // only other name for them is the subject ("βιντεοοο"): each link
        // is named by its own line (P4.34). A numbered section's real title
        // still names all its links, even a generic one ("ΣΥΝΕΝΤΕΥΞΗ ΤΥΠΟΥ
        // ΔΗΜΑΡΧΟΥ"); its lines describe the clips ("Πρώτα αυτό, είναι
        // επείγον"). A generic title is the meter's to flag, not theirs.
        let section_names_them = kw_source == KeywordSource::Title && section_kw.is_some();
        let captions: HashMap<String, String> = if many && !section_names_them {
            let found = link_captions(&s.lines, &own_labels);
            let usable = selected.iter().filter(|(u, _, _)| found.contains_key(u)).count();
            if usable >= 2 { found } else { HashMap::new() }
        } else {
            HashMap::new()
        };
        let jobs = selected
            .into_iter()
            .enumerate()
            .map(|(i, (url, tier, marker))| {
                let penalty = if unresolved { 0.2 } else { 0.0 };
                let hint = if tier == Tier::Tier2 && video_hinted.contains(&url) { 0.1 } else { 0.0 };
                let confidence = round2(base_confidence(tier, marker) + hint - penalty);
                let (keyword, keyword_from) = if let Some(k) = captions.get(&url) {
                    (k.clone(), KeywordSource::Caption)
                } else if let Some(k) = section_kw.clone() {
                    (k, kw_source)
                } else if let Some(k) = keyword_from_url_with(&url, &own_labels) {
                    (k, KeywordSource::Url)
                } else {
                    ("ASSET".into(), KeywordSource::Fallback)
                };
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
                    max_videos: if matches!(tier, Tier::Tier2 | Tier::Other) { first_n } else { None },
                    keyword_from,
                }
            })
            .collect();

        section_sources.push(kw_source);
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
    for a in mail.attachments.iter().filter(|_| !quiet) {
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
            // The attachments inherit the mail's subject or first section's
            // name: judged as that, not as a file name (P4.34).
            inherited_from = section_sources.first().copied().unwrap_or_default();
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
                keyword_from: if keyword_from_filename(&a.name, &own_labels).is_some() {
                    KeywordSource::FileName
                } else if section_kw.is_some() {
                    inherited_from
                } else {
                    KeywordSource::Fallback
                },
                keyword: keyword_from_filename(&a.name, &own_labels)
                    .or_else(|| section_kw.clone())
                    .unwrap_or_else(|| "ASSET".into()),
                confidence,
                status: status_for(Tier::Attachment, confidence, cfg),
                marker: false,
                attachment_id: Some(a.id.clone()),
                max_videos: None,
            });
        }
    }

    let job_count: usize = sections.iter().map(|s| s.jobs.len()).sum();
    if quiet {
        let detail = unrecognised_file_link.then(|| warnings::LOCKER_NOTICE_FILE_LINK.to_string());
        warnings.push(Warning::new(warnings::LOCKER_NOTICE, detail));
    }
    if held_unknown {
        warnings.push(Warning::new(warnings::OUTSIDER_UNKNOWN_SITES, None));
    }
    let outcome = if job_count > 0 {
        Outcome::Jobs
    } else if images > 0 && !quiet {
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

    /// P15.8: two staff called Aria, one Giannis, and the desk. Invented.
    fn namesake_roster() -> Vec<Journalist> {
        let j = |surname: &str, full: &str, email: &str| Journalist {
            id: 0,
            surname: surname.into(),
            full_name: full.into(),
            emails: vec![email.into()],
            default_priority: 0,
            aliases: vec![],
            groups: vec![],
            created_at: None,
        };
        vec![
            j("MCR", "Master Control Room", "master@example.gr"),
            j("DRAKAKIS", "Γιάννης Δρακάκης", "g.drakakis@example.gr"),
            j("LEFAKI", "Άρια Λεφάκη", "a.lefaki@example.gr"),
            j("MPOURNAKI", "Άρια Μπουρνάκη", "a.mpournaki@example.gr"),
            j("VARDAKIS", "Nikos Vardakis", "n.vardakis@example.gr"),
            j("VARDAKI", "Eleni Vardaki", "e.vardaki@example.gr"),
            j("KOKKINAKI", "Φωτεινή Κοκκινάκη", "f.kokkinaki@example.gr"),
            j("MANOLI", "Μαρία Μανώλη", "m.manoli@example.gr"),
            j("STAMATAKIS", "Αντώνης Σταματάκης", "a.stamatakis@example.gr"),
            Journalist { aliases: vec!["ΚΩΣΤΑ".into()], ..j("DIMITRIOU", "Kostas Dimitriou", "k.dimitriou@example.gr") },
        ]
    }

    /// P15.8 (i): «ΘΕΜΑΤΑ ΜΑΚΕΤΑ ΓΙΑΝΝΗΣ ΚΟΥΝΑΛΑΚΗΣ» is about a Giannis whose
    /// surname is not Drakakis: the first name does not take the mail from its sender.
    #[test]
    fn a_first_name_before_someone_elses_surname_is_not_that_person() {
        let r = namesake_roster();
        let link = "https://www.youtube.com/watch?v=abcdefghijk";
        let m = mail("ΘΕΜΑΤΑ ΜΑΚΕΤΑ ΓΙΑΝΝΗΣ ΚΟΥΝΑΛΑΚΗΣ", "a.lefaki@example.gr", link);
        assert_eq!(resolved(&m, &r), ("LEFAKI".into(), Resolution::Sender));
        // From the desk, nobody: the subject names no one on the roster.
        let m = mail("ΘΕΜΑΤΑ ΜΑΚΕΤΑ ΓΙΑΝΝΗΣ ΚΟΥΝΑΛΑΚΗΣ", "master@example.gr", link);
        assert_eq!(resolved(&m, &r), ("MCR".into(), Resolution::Unresolved));

        // His own surname after it, in either case: him.
        for subject in ["ΓΙΑ ΓΙΑΝΝΗ ΔΡΑΚΑΚΗ", "ΘΕΜΑΤΑ ΓΙΑΝΝΗΣ ΔΡΑΚΑΚΗΣ"] {
            let m = mail(subject, "a.lefaki@example.gr", link);
            assert_eq!(resolved(&m, &r), ("DRAKAKIS".into(), Resolution::Subject), "{subject}");
        }
        // A first name alone, or before a word that is no surname: him, as before.
        for subject in ["για Γιάννη", "ΓΙΑ ΓΙΑΝΝΗ ΠΛΑΝΑ", "ΓΙΑ ΓΙΑΝΝΗ ΤΟ ΒΙΝΤΕΟ", "ΓΙΑ ΓΙΑΝΝΗ 2"] {
            let m = mail(subject, "a.lefaki@example.gr", link);
            assert_eq!(resolved(&m, &r), ("DRAKAKIS".into(), Resolution::Subject), "{subject}");
        }
        // Her own surname with a doubled letter written single, his with a
        // letter wrong (both still end like a surname): still them.
        let m = mail("για ΦΩΤΕΙΝΗ ΚΟΚΙΝΑΚΗ", "a.lefaki@example.gr", link);
        assert_eq!(resolved(&m, &r), ("KOKKINAKI".into(), Resolution::Subject));
        for subject in ["ΓΙΑ ΓΙΑΝΝΗ ΔΡΑΓΑΚΗ", "ΘΕΜΑΤΑ ΓΙΑΝΝΗΣ ΔΡΑΓΑΚΗΣ"] {
            let m = mail(subject, "a.lefaki@example.gr", link);
            assert_eq!(resolved(&m, &r), ("DRAKAKIS".into(), Resolution::Subject), "{subject}");
        }
        // A topic after the first name, or a word set apart by punctuation: her or him.
        for (subject, who) in [
            ("ΓΙΑ ΓΙΑΝΝΗ ΑΓΙΟΣ ΝΙΚΟΛΑΟΣ", "DRAKAKIS"),
            ("ΓΙΑ ΜΑΡΙΑ - ΚΑΙΡΟΣ", "MANOLI"),
            ("ΓΙΑ ΜΑΡΙΑ: ΜΕΤΑΝΑΣΤΕΣ", "MANOLI"),
            ("ΓΙΑ ΓΙΑΝΝΗ ΣΕΙΣΜΟΣ", "DRAKAKIS"),
        ] {
            let m = mail(subject, "a.lefaki@example.gr", link);
            assert_eq!(resolved(&m, &r), (who.into(), Resolution::Subject), "{subject}");
        }
        // An alias that is the first name in another case («ΚΩΣΤΑ» for Kostas)
        // is a first name: before a guest's surname it is not him.
        let m = mail("ΓΙΑ ΚΩΣΤΑ ΘΕΟΦΙΛΟΥ", "a.lefaki@example.gr", link);
        assert_eq!(resolved(&m, &r), ("LEFAKI".into(), Resolution::Sender));
        let m = mail("ΓΙΑ ΚΩΣΤΑ", "a.lefaki@example.gr", link);
        assert_eq!(resolved(&m, &r), ("DIMITRIOU".into(), Resolution::Subject));
        // Words without a surname's ending: «live», a town, a part, a disaster.
        for subject in ["ΓΙΑ ΓΙΑΝΝΗ ΛΑΙΒ", "για Γιάννη λάιβ", "ΓΙΑ ΓΙΑΝΝΗ ΣΗΤΕΙΑ", "ΓΙΑ ΓΙΑΝΝΗ ΤΕΛΕΥΤΑΙΟ ΜΕΡΟΣ", "ΓΙΑ ΓΙΑΝΝΗ ΚΑΤΑΣΤΡΟΦΗ"] {
            let m = mail(subject, "a.lefaki@example.gr", link);
            assert_eq!(resolved(&m, &r), ("DRAKAKIS".into(), Resolution::Subject), "{subject}");
        }
        // A guest's surname (-ΑΚΗ, -ΟΥ, -ΩΦ, -ΗΣ): not him.
        for subject in ["ΚΑΛΕΣΜΕΝΟ ΓΙΑ ΓΙΑΝΝΗ ΣΚΟΥΛΑΚΗ", "ΓΙΑ ΓΙΑΝΝΗ ΘΕΟΦΙΛΟΥ", "ΓΙΑ ΓΙΑΝΝΗ ΠΕΤΡΩΦ", "ΘΕΜΑΤΑ ΓΙΑΝΝΗΣ ΣΚΟΥΛΑΚΗΣ"] {
            let m = mail(subject, "a.lefaki@example.gr", link);
            assert_eq!(resolved(&m, &r), ("LEFAKI".into(), Resolution::Sender), "{subject}");
        }
        // The word after the first name is another roster surname: that person.
        let m = mail("ΓΙΑ ΓΙΑΝΝΗ ΜΠΟΥΡΝΑΚΗ", "a.lefaki@example.gr", link);
        assert_eq!(resolved(&m, &r), ("MPOURNAKI".into(), Resolution::Subject));
        // A surname still overrides a rostered sender («για Χ»).
        let m = mail("ΓΙΑ ΔΡΑΚΑΚΗ", "a.lefaki@example.gr", link);
        assert_eq!(resolved(&m, &r), ("DRAKAKIS".into(), Resolution::Subject));
    }

    /// P15.8: the topic words rule 2 skips after a first name are still the
    /// subject's keyword after «ΓΙΑ» alone.
    #[test]
    fn a_topic_after_gia_is_still_the_keyword() {
        let r = roster();
        for (subject, keyword) in [("ΠΛΑΝΑ ΓΙΑ ΤΡΟΧΑΙΟ", "TROCHAIO"), ("ΓΙΑ ΕΚΛΟΓΕΣ", "EKLOGES"), ("ΒΙΝΤΕΟ ΓΙΑ ΚΑΚΟΚΑΙΡΙΑ", "KAKOKAIRIA")] {
            let p = parse(&mail(subject, "a.papadaki@example.gr", "https://www.youtube.com/watch?v=abcdefghijk"), &r, &ParserConfig::default());
            let got = p.sections.first().and_then(|s| s.jobs.first()).map(|j| j.keyword.as_str());
            assert_eq!(got, Some(keyword), "{subject}: {:?}", p.sections);
        }
    }

    /// P15.8 (ii): «ΓΙΑ ΑΡΙΑ» from one of the two Arias is hers, even when the
    /// forwarded headers name the other.
    #[test]
    fn the_sender_wins_a_first_name_she_shares() {
        let r = namesake_roster();
        let body = "Από: Άρια Μπουρνάκη <a.mpournaki@example.gr>\n\
                    Προς: Master Desk <master@example.gr>\n\
                    https://www.youtube.com/watch?v=abcdefghijk";
        let m = mail("Πρ: ΓΙΑ ΑΡΙΑ", "a.lefaki@example.gr", body);
        assert_eq!(resolved(&m, &r), ("LEFAKI".into(), Resolution::Subject));
        // From the desk, the headers still settle it.
        let m = mail("Πρ: ΓΙΑ ΑΡΙΑ", "master@example.gr", body);
        assert_eq!(resolved(&m, &r), ("MPOURNAKI".into(), Resolution::Subject));
        // From someone who is neither, the headers settle it too.
        let m = mail("Πρ: ΓΙΑ ΑΡΙΑ", "g.drakakis@example.gr", body);
        assert_eq!(resolved(&m, &r), ("MPOURNAKI".into(), Resolution::Subject));
    }

    /// P15.8: a transfer service's delivery is the named sender's, by surname
    /// or full name, never by a first name alone.
    #[test]
    fn a_locker_delivery_is_the_named_senders() {
        let r = namesake_roster();
        let ix = Roster::new(&r);
        assert_eq!(ix.by_full_or_surname("Nikos Vardakis"), Some("VARDAKIS".into()));
        assert_eq!(ix.by_full_or_surname("ELENI VARDAKI"), Some("VARDAKI".into()));
        assert_eq!(ix.by_full_or_surname("Ο Γιάννης Δρακάκης"), Some("DRAKAKIS".into()));
        assert_eq!(ix.by_full_or_surname("Giannis"), None, "a first name alone");
        assert_eq!(ix.by_full_or_surname("Άρια"), None, "a first name alone");
        assert_eq!(ix.by_full_or_surname("Petros Paradeigma"), None);
        // A first name is not a surname in another case: «Manolis» is not ΜΑΝΩΛΗ.
        assert_eq!(ix.by_full_or_surname("Manolis Kalogerakis"), None);
        // Someone else's first name on the roster: someone else of that surname.
        assert_eq!(ix.by_full_or_surname("Giannis Vardakis"), None);
        // Their own first name in another form: them.
        assert_eq!(ix.by_full_or_surname("Antonios Stamatakis"), Some("STAMATAKIS".into()));
        assert_eq!(ix.by_full_or_surname("ANTONIS STAMATAKIS"), Some("STAMATAKIS".into()));

        let link = "Download your files: https://www.transfernow.net/dl/20261124AbCdEf48/XyZ48key";
        let notice = |subject: &str| mail(subject, "noreply@transfernow.net", link);
        assert_eq!(resolved(&notice("Γιάννης Δρακάκης has sent you \"x.mp4\" via TransferNow"), &r), ("DRAKAKIS".into(), Resolution::LockerSender));
        assert_eq!(resolved(&notice("Ο Γιάννης Δρακάκης σας έστειλε αρχεία μέσω της TransferNow"), &r), ("DRAKAKIS".into(), Resolution::LockerSender));
        assert_eq!(resolved(&notice("Giannis has sent you \"x.mp4\" via TransferNow"), &r), ("MCR".into(), Resolution::Unresolved));
        assert_eq!(resolved(&notice("Manolis Other has sent you \"x.mp4\" via TransferNow"), &r), ("MCR".into(), Resolution::Unresolved));
        assert_eq!(resolved(&notice("Giannis Vardakis has sent you \"x.mp4\" via TransferNow"), &r), ("MCR".into(), Resolution::Unresolved));
        assert_eq!(resolved(&notice("Antonios Stamatakis has sent you \"x.mp4\" via TransferNow"), &r), ("STAMATAKIS".into(), Resolution::LockerSender));
        // About a transfer we sent, naming a rostered address: no one's.
        let m = mail("x.mp4 has been downloaded by n.vardakis@example.gr", "noreply@wetransfer.com", "x.mp4 has been downloaded.");
        assert_eq!(resolved(&m, &r), ("MCR".into(), Resolution::Unresolved));
        // An explicit subject name outranks it.
        let m = mail("Γιάννης Δρακάκης has sent you ΓΙΑ ΛΕΦΑΚΗ via TransferNow", "noreply@transfernow.net", link);
        assert_eq!(resolved(&m, &r), ("LEFAKI".into(), Resolution::Subject));
        // A staff member forwarding such a notice is the sender, as before.
        let m = mail("FW: Nikos Vardakis has sent you files", "a.lefaki@example.gr", link);
        assert_eq!(resolved(&m, &r), ("LEFAKI".into(), Resolution::Sender));
    }

    /// P15.8: a delivery that names its sender by address is that address's
    /// owner on the roster; the address outranks a display name.
    #[test]
    fn a_locker_delivery_naming_an_address_is_its_owners() {
        let r = namesake_roster();
        let swiss = |body: &str| mail("You have received 3 files via SwissTransfer", "noreply@swisstransfer.com", body);
        let m = swiss("3 files, 1.2 GB\nFrom n.vardakis@example.gr\nDownload files");
        assert_eq!(resolved(&m, &r), ("VARDAKIS".into(), Resolution::LockerSender));
        let m = mail("☁ ΠΛΗΜΜΥΡΑ ΧΑΝΙΑ", "noreply@mail.myairbridge.com", "Sender: e.vardaki@example.gr\nDownload: https://mab.to/t/AbC123/eu1");
        assert_eq!(resolved(&m, &r), ("VARDAKI".into(), Resolution::LockerSender));

        // The name says one person, the address another: the address.
        let m = mail("Nikos Vardakis <e.vardaki@example.gr> sent you 1 file", "noreply@wetransfer.com", "Download");
        assert_eq!(resolved(&m, &r), ("VARDAKI".into(), Resolution::LockerSender));

        // An address nobody on the roster has: MCR.
        let m = swiss("From someone@studio.example\nDownload files");
        assert_eq!(resolved(&m, &r), ("MCR".into(), Resolution::Unresolved));
        // One of our own domain, not on the roster: still a delivery, still MCR.
        let m = swiss("From x.unknown@example.gr\nDownload files");
        let own = vec!["master@example.gr".to_string()];
        assert_eq!(
            crate::locker_notice::notice_kind(&m, &own),
            crate::locker_notice::NoticeKind::Delivery { sender: Some("x.unknown@example.gr".into()) }
        );
        assert_eq!(resolved(&m, &r), ("MCR".into(), Resolution::Unresolved));
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

    #[test]
    fn first_n_videos_is_read_only_as_an_instruction() {
        for (line, n) in [
            ("ΓΙΑ ΠΛΑΝΑ: 2 ΠΡΩΤΑ ΒΙΝΤΕΟ", 2),
            ("τα δύο πρώτα βίντεο", 2),
            ("ΤΑ 3 ΠΡΩΤΑ", 3),
            ("Θέλω τα 3 πρώτα πλάνα", 3),
            ("ΜΟΝΟ ΤΟ ΠΡΩΤΟ ΒΙΝΤΕΟ", 1),
            ("μόνο το πρώτο", 1),
            ("first 2 videos please", 2),
        ] {
            assert_eq!(first_n_videos(line), Some(n), "{line}");
        }
        for line in [
            "Τα 2 πρώτα γκολ του Ολυμπιακού",
            "ΣΤΙΣ 3 ΠΡΩΤΕΣ ΘΕΣΕΙΣ ΤΗΣ ΒΑΘΜΟΛΟΓΙΑΣ",
            "Το πρώτο βίντεο είναι καλύτερο",
            "ΓΙΑ ΠΛΑΝΑ: ΒΙΝΤΕΟ ΑΠΟ ΙΝΣΤΑΓΚΡΑΜ",
            "ΓΙΑ ΠΛΑΝΑ: 99 ΠΡΩΤΑ ΒΙΝΤΕΟ",
        ] {
            assert_eq!(first_n_videos(line), None, "{line}");
        }
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
    fn stretched_and_generic_words_name_nothing() {
        assert_eq!(fold_stretched("VINTEOOO"), "VINTEO");
        assert_eq!(fold_stretched("KAAALIMERA"), "KALIMERA");
        assert_eq!(fold_stretched("ANNA2000"), "ANNA2000", "digits and doubled letters stay");
        for k in ["TREILER", "VINTEOOO", "PLANAVINTEO", "YLIKO", "VIRAL"] {
            assert!(is_generic_keyword(k), "{k}");
        }
        for k in ["FIKTAKIS", "DUNE", "SEISMOSSITEIA", "AEKOFI"] {
            assert!(!is_generic_keyword(k), "{k}");
        }
        assert_eq!(keyword_from_title("Dune: Part Three | Official Trailer").as_deref(), Some("DUNEPART"));
        assert_eq!(keyword_from_title("Official Trailer 2 (HD)"), None, "only the kind of clip is left");
        // Instagram's login wall answered a plain request with this title.
        assert_eq!(keyword_from_title("Instagram"), None);
        assert_eq!(keyword_from_title("Log in • Instagram"), None);
    }

    #[test]
    fn a_models_keyword_is_spelled_by_our_elot_not_its_own() {
        // Gemini wrote "AEKOFY" and "EREUNITRIA"; ELOT 743 is OFI, EREVNITRIA.
        assert_eq!(keyword_from_model("ΑΕΚ ΟΦΗ").as_deref(), Some("AEKOFI"));
        assert_eq!(keyword_from_model("Ερευνήτρια Ρωσία").as_deref(), Some("EREVNITRIAROSIA"));
        assert_eq!(keyword_from_model("The Social Reckoning").as_deref(), Some("THESOCIALRECKONING"));
        assert_eq!(keyword_from_model("ΣΥΝΕΝΤΕΥΞΗ ΤΥΠΟΥ ΠΕΡΙΦΕΡΕΙΑΡΧΗ").map(|k| k.len()), Some(20), "capped like every keyword");
        for refused in ["TRAILER", "τρέιλερ", "null", "", "Βίντεοοο"] {
            assert_eq!(keyword_from_model(refused), None, "{refused}");
        }
    }

    #[test]
    fn attachments_that_only_inherit_the_subject_are_judged_by_it() {
        // The desk's "ΠΛΑΝΑ ΚΑΙ ΣΤΟΙΧΕΙΑ ΕΠΙΚΟΙΝΩΝΙΑΣ ΓΙΑ ΣΗΜΕΡΙΝΟ ΚΑΛΕΣΜΕΝΟ
        // ΣΤΕΛΙΟ ΦΙΚΤΑΚΗ": five attached clips, all STOICHEIAEPIKOINONIA.
        let mut m = mail("ΠΛΑΝΑ ΚΑΙ ΣΤΟΙΧΕΙΑ ΕΠΙΚΟΙΝΩΝΙΑΣ ΓΙΑ ΣΗΜΕΡΙΝΟ ΚΑΛΕΣΜΕΝΟ ΣΤΕΛΙΟ ΦΙΚΤΑΚΗ", "a.papadaki@example.gr", "Καλησπέρα");
        for (i, name) in ["VID_20261006_101010.mp4", "VID_20261006_101511.mp4", "Λιμάνι Χανίων.mp4"].iter().enumerate() {
            m.attachments.push(crate::mail::AttachmentMeta {
                id: i.to_string(),
                name: name.to_string(),
                content_type: "video/mp4".into(),
                size: 1000,
            });
        }
        let p = parse(&m, &roster(), &ParserConfig::default());
        assert!(!p.has_warning(warnings::JOURNALIST_SUGGESTED), "ΣΗΜΕΡΙΝΟ is a day, not a name: {:?}", p.warnings);
        let v = keyword_verdicts(&p);
        let by = |i: &str| v.iter().find(|x| x.index_str == i).unwrap();
        assert!(by("1A").is_low() && by("1A").from == KeywordSource::Subject, "{:?}", by("1A"));
        assert!(by("1B").is_low());
        assert_eq!((by("1C").keyword.as_str(), by("1C").from), ("LIMANICHANION", KeywordSource::FileName));
        assert!(!by("1C").is_low(), "a file name that names something stands");
    }

    #[test]
    fn the_keyword_meter_flags_names_that_cannot_tell_videos_apart() {
        let v = |subject: &str, body: &str| keyword_verdicts(&parse(&mail(subject, "a.papadaki@example.gr", body), &roster(), &ParserConfig::default()));

        // P4.34, from the desk: four different films, one subject word.
        let trailers = v(
            "Πρ: τρέιλερ 9-10",
            "Καλημέρα κι ευχαριστώ!\nhttps://youtu.be/aaaaaaaaaa1\nhttps://youtu.be/aaaaaaaaaa2\nhttps://youtu.be/aaaaaaaaaa3\nhttps://youtu.be/aaaaaaaaaa4",
        );
        assert_eq!(trailers.len(), 4);
        for t in &trailers {
            assert!(t.is_low(), "{t:?}");
            assert!(t.reasons.iter().any(|r| r == "generic"), "{t:?}");
            assert!(t.reasons.iter().any(|r| r == "shared by 4 links"), "{t:?}");
        }

        // Two words kept of a long subject about a guest.
        let guest = v("ΠΛΑΝΑ ΚΑΙ ΣΤΟΙΧΕΙΑ ΕΠΙΚΟΙΝΩΝΙΑΣ ΓΙΑ ΣΗΜΕΡΙΝΟ ΚΑΛΕΣΜΕΝΟ ΣΤΕΛΙΟ ΦΙΚΤΑΚΗ", "https://youtu.be/bbbbbbbbbb1");
        assert!(guest[0].is_low(), "{:?}", guest[0]);

        // One subject naming one link is fine.
        let one = v("Σεισμός στη Σητεία", "https://youtu.be/cccccccccc1");
        assert!(!one[0].is_low(), "{:?}", one[0]);

        // A numbered title names all its links; sharing it is the design.
        let numbered = v("Θέματα", "1. ΣΕΙΣΜΟΣ ΣΤΗ ΣΗΤΕΙΑ\nhttps://youtu.be/dddddddddd1\nhttps://youtu.be/dddddddddd2");
        assert!(numbered.iter().all(|x| !x.is_low() && x.from == KeywordSource::Title), "{numbered:?}");

        // A line above each link names it.
        let captions = v("βιντεοοο", "Διαγωνισμός ύπνου\nhttps://youtu.be/eeeeeeeeee1\nΧορός στη Σητεία\nhttps://youtu.be/eeeeeeeeee2");
        assert_eq!(captions.iter().map(|x| x.keyword.as_str()).collect::<Vec<_>>(), ["DIAGONISMOSYPNOU", "CHOROSSITEIA"]);
        assert!(captions.iter().all(|x| x.from == KeywordSource::Caption && !x.is_low()));
    }

    #[test]
    fn a_word_the_sender_glued_to_a_link_is_not_part_of_it() {
        // P3.9: one sender glued "-ΒΙΝΤΕΟ" / "-ΦΩΤΟ" / "-BINTEO" to every
        // link; news247 answers 404 for the decorated address.
        let cut = |u: &str| trailing_annotation(u).map(|i| u[..i].to_string());
        assert_eq!(cut("https://www.news247.gr/kosmos/arthro/-ΒΙΝΤΕΟ").as_deref(), Some("https://www.news247.gr/kosmos/arthro/"));
        assert_eq!(cut("https://www.news247.gr/kosmos/arthro/-ΦΩΤΟ").as_deref(), Some("https://www.news247.gr/kosmos/arthro/"));
        assert_eq!(cut("https://www.youtube.com/watch?v=u2BU7HFTb54-BINTEO").as_deref(), Some("https://www.youtube.com/watch?v=u2BU7HFTb54"));
        assert_eq!(cut("https://www.amna.gr/home/videos/1/Title-o-P-Name-ΒΙΝΤΕΟ").as_deref(), Some("https://www.amna.gr/home/videos/1/Title-o-P-Name"));
        assert_eq!(cut("https://site.gr/a/arthroΒΙΝΤΕΟ").as_deref(), Some("https://site.gr/a/arthro"), "Greek capitals: no separator needed");
        assert_eq!(cut("https://site.gr/a/arthro-ΒΙΝΤΕΟ+ΦΩΤΟ").as_deref(), Some("https://site.gr/a/arthro"));
        assert_eq!(cut("https://site.gr/a/arthro-%CE%92%CE%99%CE%9D%CE%A4%CE%95%CE%9F").as_deref(), Some("https://site.gr/a/arthro"));
        assert_eq!(cut("https://site.gr/a/arthro-%ce%a6%ce%a9%ce%a4%ce%9f").as_deref(), Some("https://site.gr/a/arthro"));
        // Real addresses that only look similar stay whole.
        assert_eq!(cut("https://www.protothema.gr/world/to-viral-binteo/"), None, "a lowercase slug is the site's own");
        assert_eq!(cut("https://www.star.gr/tv/clip-VIDEO"), None, "English words end real paths");
        assert_eq!(cut("https://site.gr/a/MYBINTEO"), None, "a Latin word needs a separator");
        assert_eq!(cut("https://site.gr/-ΒΙΝΤΕΟ"), None, "nothing but the front page would be left");
    }

    /// P15.5: photo links without an image extension (a mail replay sent four
    /// of them to review) are photos by their server or their query; a video
    /// host, a lookalike and an article about images are not.
    #[test]
    fn known_image_servers_are_photos_without_an_extension() {
        let c = ParserConfig { tier2_domains: vec!["example-news.gr".into()], ..ParserConfig::default() };
        for url in [
            "https://images.openai.com/static-rsc-4/AbC_dEf-123?purpose=fullsize",
            "https://encrypted-tbn0.gstatic.com/images?q=tbn:ANd9GcQabc&s=10",
            "https://encrypted-tbn3.gstatic.com/images?q=tbn:ANd9GcQabc",
            "https://t0.gstatic.com/images?q=tbn:ANd9GcQabc",
            "https://i.pinimg.com/originals/ab/cd/ef/abcdef",
            "https://pbs.twimg.com/media/GabcDEF?format=jpg&name=large",
            "https://pbs.twimg.com/media/GabcDEF",
            "https://lh3.googleusercontent.com/abc=w1200-h630-p-k-no-nu",
            "https://lh6.googleusercontent.com/proxy/abc",
            "https://braze-images.com/appboy/communication/assets/image_assets/images/abc",
            "https://cdn.example.org/asset/123?fm=webp&w=800",
            "https://cdn.example.org/asset/123?format=AVIF",
            "https://www.example-news.gr/resizer/abc?format=jpeg",
        ] {
            assert_eq!(classify(url, &c), Tier::Image, "{url}");
        }
        for (url, want) in [
            ("https://video.twimg.com/ext_tw_video/1/pu/vid/1280x720/abc.mp4", Tier::Other),
            ("https://pbs.twimg.com/ext_tw_video_thumb/1/pu/abc", Tier::Other),
            ("https://rr3---sn-abc.googlevideo.com/videoplayback?expire=1&mime=video%2Fmp4", Tier::Other),
            ("https://video.fbcdn.net/v/t42.1790-2/abc.mp4?_nc_cat=1", Tier::Other),
            ("https://scontent.xx.fbcdn.net/v/t39.25447-2/abc?format=mp4", Tier::Other),
            ("https://www.youtube.com/watch?v=abcdefghijk", Tier::Tier1),
            ("https://lh3.googleusercontent.com.evil.example/abc", Tier::Other),
            ("https://lh7.googleusercontent.com/abc", Tier::Other),
            // Google Photos video streams on the same hosts.
            ("https://lh3.googleusercontent.com/pw/AbCdEf123=dv", Tier::Other),
            ("https://lh5.googleusercontent.com/pw/AbCdEf123=m18", Tier::Other),
            ("https://lh6.googleusercontent.com/AbCdEf123=m37", Tier::Other),
            ("https://lh4.googleusercontent.com/pw/AbCdEf123=m22?format=jpg", Tier::Other),
            // P15.12b: the same options combined with size options.
            ("https://lh3.googleusercontent.com/pw/AbCdEf123=w1280-h720-dv", Tier::Other),
            ("https://lh5.googleusercontent.com/pw/AbCdEf123=m22-no", Tier::Other),
            ("https://lh6.googleusercontent.com/pw/AbCdEf123=s0-dv", Tier::Other),
            // Size options alone are a photo's.
            ("https://lh3.googleusercontent.com/pw/AbCdEf123=w1280-h720", Tier::Image),
            ("https://lh3.googleusercontent.com/pw/AbCdEf123=w1280-mx-dvd", Tier::Image),
            ("https://notimages.openai.com.example/x", Tier::Other),
            ("https://encrypted-tbnx.gstatic.com/images?q=x", Tier::Other),
            ("https://www.gstatic.com/images/branding/logo", Tier::Other),
            ("https://fonts.gstatic.com/s/roboto/v1/font", Tier::Other),
            // Q2: an X post's photo address stays a post; it may carry a video too.
            ("https://x.com/someone/status/1234567890/photo/1", Tier::Tier1),
            ("https://www.example-news.gr/news/images-of-the-storm-video", Tier::Tier2),
            ("https://www.example-news.gr/images/2026/storm?format=video", Tier::Tier2),
        ] {
            assert_eq!(classify(url, &c), want, "{url}");
        }
        // Only an extension makes a photo certain; a server match can be queued by hand.
        assert!(has_image_extension("https://www.example.org/press/photo1.JPG"));
        assert!(!has_image_extension("https://images.openai.com/static-rsc-4/abc"));
        assert!(!has_image_extension("not a url.jpg"));
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
        // ΑΠΕ-ΜΠΕ video pages are YouTube embeds the pipeline resolves; they
        // were parked as MANUAL_DOWNLOAD without ever being tried.
        assert_eq!(
            classify("https://www.amna.gr/home/videos/1028434/Proores-ekloges-ΒΙΝΤΕΟ", &c),
            Tier::Tier2
        );
        assert_eq!(classify("https://example.org/story", &c), Tier::Other);
        // Not fooled by a lookalike host.
        assert_eq!(classify("https://notyoutube.com/watch?v=x", &c), Tier::Other);
        // P15.1: SwissTransfer and MyAirBridge's short links are lockers.
        assert_eq!(classify("https://www.swisstransfer.com/d/0a1b2c3d", &c), Tier::Locker);
        assert_eq!(classify("https://mab.to/t/AbC123/eu1", &c), Tier::Locker);
    }

    /// P15.2: who is outside the newsroom, from the roster alone.
    #[test]
    fn outsiders_are_told_apart_by_roster_address_and_own_domain() {
        let person = |emails: &[&str]| Journalist {
            id: 0,
            surname: "X".into(),
            full_name: String::new(),
            emails: emails.iter().map(|s| s.to_string()).collect(),
            default_priority: 0,
            aliases: vec![],
            groups: vec![],
            created_at: None,
        };
        let roster = vec![person(&["a@example.gr"]), person(&["Someone@Gmail.com", "b@hotmail.co.uk"])];
        assert!(!is_outsider("a@example.gr", &roster), "a roster address");
        assert!(!is_outsider(" A@Example.GR ", &roster), "case and spaces");
        assert!(!is_outsider("newperson@example.gr", &roster), "the roster's own domain");
        assert!(!is_outsider("desk@news.example.gr", &roster), "a subdomain of an own domain");
        assert!(!is_outsider("someone@gmail.com", &roster), "the roster's gmail address itself");
        assert!(is_outsider("stranger@gmail.com", &roster), "a public provider is never an own domain");
        assert!(is_outsider("stranger@hotmail.co.uk", &roster), "nor any hotmail");
        assert!(is_outsider("press@municipality.example", &roster), "an unrelated domain");
        assert!(is_outsider("x@notexample.gr", &roster), "a lookalike is not a subdomain");
        assert!(is_outsider("not-an-address", &roster));
        // No own domain to tell by (fresh installation): nobody is an outsider.
        assert!(!is_outsider("a@example.gr", &[]), "an empty roster");
        assert!(!is_outsider("press@municipality.example", &[person(&[])]), "a roster without addresses");
        assert!(!is_outsider("press@municipality.example", &[person(&["x@gmail.com", "y@outlook.com"])]), "public addresses only");
        assert!(is_public_mail_domain("mail.yahoo.gr") && is_public_mail_domain("outlook.com") && !is_public_mail_domain("example.gr"));
    }

    /// P15.1: a link that does something (unsubscribe, delete, report, sign
    /// in) is never a job; a story whose slug has such a word is.
    #[test]
    fn action_links_are_recognised_by_whole_segments_and_query_keys() {
        for url in [
            "https://www.swisstransfer.com/transfers/0a1b2c3d-1111/delete?delete-signature=abc123",
            "https://www.swisstransfer.com/transfers/0a1b2c3d-1111?delete-signature=abc123",
            "https://myaccount.google.com/visitor-delete?atu=1",
            "https://newsletter.us1.list-manage.com/unsubscribe?u=abc&id=def",
            "https://www.transfernow.net/en/contact?category=ABUSE&subCat=REPORT&link=x",
            "https://news.example.org/mail?unsubscribe=1",
            "https://news.example.org/email-preferences",
            "https://news.example.org/account/Unsubscribe.php?u=1",
            "https://news.example.org/a/%75nsubscribe/1",
        ] {
            assert!(is_action_link(url), "{url} is an action link");
        }
        // Sign-in pages change nothing: they behave as before P15.1.
        for url in [
            "https://m.facebook.com/login/?next=https%3A%2F%2Fm.facebook.com%2Freel%2F123",
            "https://accounts.google.com/ServiceLogin?continue=https%3A%2F%2Fdrive.google.com%2Ffile%2Fd%2F1AbC_dEf-GhIjKlMn%2Fview",
            "https://portal.example.org/user/login?next=/x",
            "https://portal.example.org/Sign-In",
            "https://portal.example.org/logout",
            "https://portal.example.org/reset-password",
        ] {
            assert!(!is_action_link(url), "{url} is not an action link");
        }
        for url in [
            "https://www.bbc.com/news/world-delete-12345",
            "https://www.neakriti.gr/kriti/irakleio/2203079_report-ston-dromo",
            "https://www.youtube.com/watch?v=abc123",
            "https://youtu.be/abc123",
            "https://www.example.gr/login-stories/123",
            "https://www.example.gr/news/preferences-of-voters?category=politics",
            "https://www.example.gr/story?id=5&delete=no-key-name",
        ] {
            assert!(!is_action_link(url), "{url} is a story");
        }
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

    /// P15.9: a wrap-width line ending in a link, followed by a line that is
    /// itself a link, is two links; a real soft wrap still joins.
    #[test]
    fn a_link_on_the_next_line_is_its_own_link() {
        let long = "https://news.example/video/2026/10/09/some-long-slug-words-here-and-more-sp";
        assert!(long.len() >= 70);
        let urls = |body: &str| find_urls(&rejoin_wrapped_urls(body)).into_iter().map(|(_, u)| u).collect::<Vec<_>>();

        // A known host with a path, and any host-like name with a path.
        assert_eq!(urls(&format!("{long}\nyoutube.com/watch?v=AbCdEfGhIjK")), [long, "https://youtube.com/watch?v=AbCdEfGhIjK"]);
        assert_eq!(urls(&format!("{long}\nm.youtube.com/watch?v=AbCdEfGhIjK")), [long, "https://m.youtube.com/watch?v=AbCdEfGhIjK"]);
        assert_eq!(rejoin_wrapped_urls(&format!("{long}\nclips.news-two.example/v/abc-def")), format!("{long}\nclips.news-two.example/v/abc-def"));

        // A client's wrap of one long link: still one link.
        assert_eq!(urls(&format!("{long}\nabc-def/ghi?x=1")), [format!("{long}abc-def/ghi?x=1")]);
        assert_eq!(urls(&format!("{long}\nstory-title.html/amp")), [format!("{long}story-title.html/amp")], "a file name is not a host");

        // A link, a space and a lone dash: the line does not end in a link.
        assert_eq!(urls(&format!("{long} -\nabc-def/ghi")), [long]);
        assert_eq!(urls(&format!("{long} –\nabc-def/ghi")), [long]);
    }

    /// P15.9: a dash that is a whole last path segment (`…/-`) is not the
    /// link's; one at the end of a slug, in a query or in a fragment is.
    #[test]
    fn a_dash_as_the_last_path_segment_is_dropped() {
        assert_eq!(clean_url("https://news.example/a-b-c/-").as_deref(), Some("https://news.example/a-b-c/"));
        assert_eq!(clean_url("https://news.example/4812330/-").as_deref(), Some("https://news.example/4812330/"));
        assert_eq!(clean_url("www.news.example/a-b-c/-").as_deref(), Some("https://www.news.example/a-b-c/"));
        // A slug that ends in a dash keeps it: sites that cut long titles keep
        // the dash in the real address, and the link without it is a 404.
        assert_eq!(clean_url("https://news.example/a-b-c-").as_deref(), Some("https://news.example/a-b-c-"));
        assert_eq!(clean_url("https://news.example/watch?v=ab-").as_deref(), Some("https://news.example/watch?v=ab-"));
        assert_eq!(clean_url("https://news.example/a#part-").as_deref(), Some("https://news.example/a#part-"));
        assert_eq!(clean_url("https://news.example/a/?q=-").as_deref(), Some("https://news.example/a/?q=-"));
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

    /// P15.10: file types, camera file names, numbers and transfer-notice
    /// words are never part of a keyword; with nothing else left, the next
    /// source names the file.
    #[test]
    fn junk_never_makes_a_keyword() {
        for junk in ["ΠΛΑΝΑ MP4", "FILE382", "6912345678", "IMG_4411.MOV", "20261027 14:30", "27/10/2026", "2026-10-27", "has sent you", "ASSET"] {
            assert_eq!(keyword_from_text(junk), None, "{junk}");
        }
        assert_eq!(keyword_from_text("ΣΥΝΕΝΤΕΥΞΗ_ΔΗΜΑΡΧΟΥ.MP4").as_deref(), Some("SYNENTEFXIDIMARCHOU"));
        assert_eq!(keyword_from_text("ΔΗΜΟΤΙΚΟ 20261027 ΣΥΜΒΟΥΛΙΟ").as_deref(), Some("DIMOTIKOSYMVOULIO"));
        assert_eq!(keyword_from_text("ΣΥΜΒΟΥΛΙΟ 27/10 14:30").as_deref(), Some("SYMVOULIO"));
        assert_eq!(keyword_from_text("COVID19 ΕΜΒΟΛΙΑ").as_deref(), Some("COVID19EMVOLIA"), "a few digits in a word stay");
        // Short numbers and codes are topics (P15.10 round 3).
        for (text, keyword) in [
            ("ΜΗΝΥΜΑ 112", "MINYMA112"),
            ("ΑΡΚΑΔΙ 1866", "ARKADI1866"),
            ("ΔΕΛΤΙΟ 20Η", "DELTIO20I"),
            ("F35 ΣΟΥΔΑ", "F35SOUDA"),
            ("A320 ΗΡΑΚΛΕΙΟ", "A320IRAKLEIO"),
        ] {
            assert_eq!(keyword_from_text(text).as_deref(), Some(keyword), "{text}");
        }
        // ΒΙΑ is VIA, ΦΙΛΕΣ is FILES: a notice word only in a service's own mail.
        assert_eq!(keyword_from_text("ΕΝΔΟΟΙΚΟΓΕΝΕΙΑΚΗ ΒΙΑ ΣΤΟ ΗΡΑΚΛΕΙΟ").as_deref(), Some("ENDOOIKOGENEIAKIVIA"));
        assert_eq!(keyword_from_text("ΣΧΟΛΙΚΗ ΒΙΑ").as_deref(), Some("SCHOLIKIVIA"));
        assert_eq!(valid_keyword("SCHOLIKIVIA").as_deref(), Some("SCHOLIKIVIA"));
        assert_eq!(valid_keyword("VIA").as_deref(), Some("VIA"));
        // The LLM's keywords too.
        for junk in ["MP4", "HASSENT", "FILE382", "6912345678", "MP4MOV"] {
            assert_eq!(valid_keyword(junk), None, "{junk}");
        }
        assert_eq!(valid_keyword("FESTIVAL").as_deref(), Some("FESTIVAL"));

        let r = roster();
        let keyword = |subject: &str, from: &str| {
            let p = parse(&mail(subject, from, "https://www.youtube.com/watch?v=abcdefghijk"), &r, &ParserConfig::default());
            p.sections.first().and_then(|s| s.jobs.first()).map(|j| j.keyword.clone()).unwrap()
        };
        // Nothing left in the subject: the fallback, as before.
        assert_eq!(keyword("ΠΛΑΝΑ MP4", "a.papadaki@example.gr"), "ASSET");
        assert_eq!(keyword("ΓΙΑ ΔΗΜΟ 20261027", "a.papadaki@example.gr"), "DIMO");
        // A label of the station's own mail domain (example.gr, from the roster).
        assert_eq!(keyword("ΘΕΜΑΤΑ EXAMPLE ΣΕΙΣΜΟΣ", "a.papadaki@example.gr"), "SEISMOS");

        // A transfer notice: the file's words, not "HASSENT".
        let r = namesake_roster();
        let m = mail(
            "Nikos Vardakis has sent you festival-crete.mp4 via TransferNow",
            "noreply@transfernow.net",
            "Download your files: https://www.transfernow.net/dl/20261127AbCd/XyZkey",
        );
        let p = parse(&m, &r, &ParserConfig::default());
        assert_eq!(p.journalist.surname, "VARDAKIS");
        assert_eq!(p.sections[0].jobs[0].keyword, "FESTIVALCRETE", "{:?}", p.sections);
    }

    /// P15.10 round 2: a transfer notice's sender is not the keyword, known
    /// to the roster or not; the file's words are. English filler names nothing.
    #[test]
    fn a_notice_is_named_by_its_file_not_its_sender() {
        let r = roster();
        let link = "Download your files: https://www.transfernow.net/dl/20261127AbCd/XyZkey";
        let notice = |subject: &str, body: &str| {
            let p = parse(&mail(subject, "noreply@transfernow.net", body), &r, &ParserConfig::default());
            (p.journalist.surname.clone(), p.sections[0].jobs[0].keyword.clone())
        };
        // Nobody on the roster: MCR, and the subject's file name.
        assert_eq!(notice("Maria Testaki has sent you festival-crete.mp4 via TransferNow", link), ("MCR".into(), "FESTIVALCRETE".into()));
        // Nothing but the notice in the subject: the body's file name, not the
        // sender line above it (address and all).
        let body = format!("Maria Testaki <m.testaki@studio.example> sent you 1 file\nplimmyra_limani.mp4 (1.1 GB)\n{link}");
        assert_eq!(notice("Maria Testaki has sent you files via TransferNow", &body), ("MCR".into(), "PLIMMYRALIMANI".into()));

        assert_eq!(keyword_from_text("Here is the video you might like").as_deref(), Some("LIKE"), "only a word that is not filler");
        assert_eq!(keyword_from_text("Please click here to view your items"), None);
    }

    /// P15.10 round 3: a transfer service's own words never name a clip in
    /// its mail, and stay a topic in anyone else's.
    #[test]
    fn a_transfer_services_words_name_nothing_in_its_own_mail() {
        let r = roster();
        let section_keyword = |subject: &str, from: &str, body: &str| {
            parse(&mail(subject, from, body), &r, &ParserConfig::default()).sections.first().and_then(|s| s.keyword.clone())
        };
        let link = "https://we.tl/t-AbCdEf1234";
        assert_eq!(section_keyword("p.example@studio.example έκανε μεταφόρτωση μέσω WeTransfer", "noreply@wetransfer.com", link), None);
        // The sender's name is dropped too (round 2): nothing is left.
        assert_eq!(section_keyword("Ο Πέτρος σας έστειλε αρχεία μέσω WeTransfer", "noreply@wetransfer.com", link), None);
        assert_eq!(section_keyword("Your transfer expires on 3 November", "noreply@wetransfer.com", link), None);
        assert_eq!(section_keyword("Η μεταφορά λήγει σε 2 ημέρες, 3 Νοεμβρίου", "noreply@transfernow.net", link), None);
        // The file's name, when the notice gives one, still names it.
        assert_eq!(
            section_keyword("Your transfer expires on 3 November", "noreply@wetransfer.com", &format!("katastrofi-limani.mp4\n{link}")),
            Some("KATASTROFILIMANI".into())
        );
        // "via" is a notice word in a service's own mail (in anyone else's it is ΒΙΑ).
        assert_eq!(section_keyword("Festival Chania via WeTransfer", "noreply@wetransfer.com", link), Some("FESTIVALCHANIA".into()));
        // In a colleague's mail the same words are the story.
        assert_eq!(
            section_keyword("ΜΕΤΑΦΟΡΑ ΑΣΘΕΝΟΥΣ ΜΕ ΕΚΑΒ", "a.papadaki@example.gr", "https://www.youtube.com/watch?v=abcdefghijk"),
            Some("METAFORAASTHENOUS".into())
        );
    }

    /// P15.10 round 3: the preamble names a link only from the lines down to
    /// it; a footer under it (Outlook's «Λήψη του Outlook για Android») does
    /// not, and the link's own slug does.
    #[test]
    fn a_footer_below_the_link_does_not_name_it() {
        let body = "Δείτε εδώ:\nhttps://news.example/kriti/seismos-irakleio-video\n\nΛήψη του Outlook για Android<https://aka.ms/AAb9ysg>\nΚαλή συνέχεια";
        let p = parse(&mail("Βίντεο", "a.papadaki@example.gr", body), &roster(), &ParserConfig::default());
        let job = p.jobs().find(|j| j.url.contains("news.example")).expect("the news link is a job");
        assert_eq!(job.keyword, "SEISMOSIRAKLEIO", "{:?}", p.sections);
    }

    /// P15.11: a FEX share from outside the newsroom is a locker for MCR,
    /// not a link to an unknown site held back.
    #[test]
    fn a_fex_share_from_an_outsider_is_a_locker() {
        let p = parse(&mail("Πλάνα από τη φωτιά", "someone@studio.example", "https://fex.net/uk/s/abc1d2e"), &roster(), &ParserConfig::default());
        let jobs: Vec<_> = p.jobs().collect();
        assert_eq!(jobs.len(), 1, "{p:?}");
        assert_eq!((jobs[0].tier, jobs[0].status), (Tier::Locker, JobStatus::ManualDownload), "{p:?}");
        assert!(!p.warnings.iter().any(|w| w.code == warnings::OUTSIDER_UNKNOWN_SITES), "{:?}", p.warnings);
        assert_eq!(classify("https://fex.net/s/abc1d2e", &ParserConfig::default()), Tier::Locker);
    }

    /// P15.11 round 2: a kDrive share typed without `https://` is a link,
    /// and a locker for MCR.
    #[test]
    fn a_bare_kdrive_share_is_found() {
        let found: Vec<String> = find_urls("Τα πλάνα: kdrive.infomaniak.com/app/share/123456/ab12cd34-ef56 ευχαριστώ").into_iter().map(|(_, u)| u).collect();
        assert_eq!(found, ["https://kdrive.infomaniak.com/app/share/123456/ab12cd34-ef56"]);
        assert_eq!(classify(&found[0], &ParserConfig::default()), Tier::Locker);
    }

    /// P15.11: a SwissTransfer delivery a colleague forwards carries
    /// Infomaniak's ads and footer: those are the service's pages, never
    /// jobs; the share is the one job, for MCR.
    #[test]
    fn a_forwarded_swisstransfer_delivery_queues_only_its_share() {
        let body = "---------- Forwarded message ---------\nFrom: SwissTransfer <noreply@swisstransfer.com>\n\
                    You have received 2 files\nhttps://www.swisstransfer.com/d/0a1b2c3d-1111-2222-3333-444455556666\n\
                    Discover kDrive https://www.infomaniak.com/en/kdrive?utm_source=swisstransfer\n\
                    https://news.infomaniak.com/l/promo-2026\nManage your account https://manager.infomaniak.com/v3/account";
        let p = parse(&mail("FW: You have received 2 files", "a.papadaki@example.gr", body), &roster(), &ParserConfig::default());
        let jobs: Vec<_> = p.jobs().collect();
        assert_eq!(jobs.len(), 1, "{p:?}");
        assert!(jobs[0].url.contains("swisstransfer.com/d/") && jobs[0].status == JobStatus::ManualDownload, "{p:?}");
        assert!(p.ignored_urls.iter().filter(|u| u.contains("infomaniak.com")).count() == 3, "{:?}", p.ignored_urls);
    }

    /// P15.11: a notice that our own transfer was downloaded, naming an
    /// address, queues nothing.
    #[test]
    fn an_outbound_notice_naming_an_address_queues_nothing() {
        let m = mail(
            "Your files were downloaded — transfer from p.example@studio.example",
            "noreply@wetransfer.com",
            "Your files were downloaded.\nhttps://wetransfer.com/downloads/a1b2c3d4e5/f6a7b8c9d0",
        );
        let p = parse(&m, &roster(), &ParserConfig::default());
        assert_eq!(p.jobs().count(), 0, "{p:?}");
    }

    /// P15.10 round 4: a section's own title line names it, as at HEAD,
    /// whatever the links below it.
    #[test]
    fn a_title_line_above_its_links_still_names_them() {
        let r = roster();
        let kw = |subject: &str, body: &str| {
            let p = parse(&mail(subject, "a.papadaki@example.gr", body), &r, &ParserConfig::default());
            (p.sections.iter().map(|s| s.keyword.clone()).collect::<Vec<_>>(), p.jobs().map(|j| j.keyword.clone()).collect::<Vec<_>>())
        };
        let (sections, jobs) = kw("ΘΕΜΑΤΑ", "ΤΟ ΔΙΝΕΙ lIVE ΕΙΚΟΝΑ ΕΚΛΟΓΕΣ - ΝΑ ΤΟ ΔΩΣΟΥΜΕ\nhttps://www.youtube.com/watch?v=abcdefghij1");
        assert_eq!(sections, [Some("DINEILIVE".to_string())], "{jobs:?}");
        let (sections, jobs) = kw("ΘΕΜΑΤΑ", "AYTOΔΙΝΕΙ lIVE ΕΙΚΟΝΑ\nhttps://www.youtube.com/watch?v=abcdefghij1");
        assert_eq!(sections, [Some("AYTODINEILIVE".to_string())], "{jobs:?}");
        let body = "για την αρχή 30 δευτερόλεπτα\nhttps://www.youtube.com/watch?v=abcdefghij1\nhttps://www.youtube.com/watch?v=abcdefghij2\nhttps://www.youtube.com/watch?v=abcdefghij3";
        let (sections, jobs) = kw("ΘΕΜΑΤΑ", body);
        assert_eq!(sections, [Some("ARCHIDEFTEROLEPTA".to_string())], "{jobs:?}");
        let (sections, _) = kw("ΘΕΜΑΤΑ", &format!("1. {}", body));
        assert_eq!(sections, [Some("ARCHIDEFTEROLEPTA".to_string())]);
        // A link above the title (a logo, a link pasted first): the title
        // still names the section, as at HEAD.
        let (sections, jobs) = kw("ΘΕΜΑΤΑ", &format!("https://news.example/logo\n{body}"));
        assert_eq!(sections, [Some("ARCHIDEFTEROLEPTA".to_string())], "{jobs:?}");
        let (sections, jobs) = kw("ΘΕΜΑΤΑ", "https://www.youtube.com/watch?v=abcdefghij0\nΤΟ ΔΙΝΕΙ lIVE ΕΙΚΟΝΑ ΕΚΛΟΓΕΣ - ΝΑ ΤΟ ΔΩΣΟΥΜΕ\nhttps://www.youtube.com/watch?v=abcdefghij1");
        assert_eq!(sections, [Some("DINEILIVE".to_string())], "{jobs:?}");
    }

    /// P15.10 round 3: a section title loses the journalist's surname, and
    /// a first name only right before it; a first name alone is a word.
    #[test]
    fn a_section_title_drops_the_journalists_surname_not_a_lone_first_name() {
        let r = vec![
            Journalist { id: 0, surname: "MCR".into(), full_name: "Master Control Room".into(), emails: vec!["master@example.gr".into()], default_priority: 0, aliases: vec![], groups: vec![], created_at: None },
            Journalist { id: 0, surname: "TESTAKI".into(), full_name: "Νίκη Τεστάκη".into(), emails: vec!["n.testaki@example.gr".into()], default_priority: 0, aliases: vec![], groups: vec![], created_at: None },
        ];
        let body = "1. ΝΙΚΗ ΟΦΗ\nhttps://www.youtube.com/watch?v=abcdefghij1\n\n2. ΤΕΣΤΑΚΗ ΣΕΙΣΜΟΣ\nhttps://www.youtube.com/watch?v=abcdefghij2\n\n3. ΝΙΚΗ ΤΕΣΤΑΚΗ ΠΛΗΜΜΥΡΑ\nhttps://www.youtube.com/watch?v=abcdefghij3";
        let p = parse(&mail("ΘΕΜΑΤΑ", "n.testaki@example.gr", body), &r, &ParserConfig::default());
        assert_eq!(p.journalist.surname, "TESTAKI");
        let keywords: Vec<Option<String>> = p.sections.iter().map(|s| s.keyword.clone()).collect();
        assert_eq!(keywords, [Some("NIKIOFI".to_string()), Some("SEISMOS".to_string()), Some("PLIMMYRA".to_string())]);
    }

    /// P15.10 round 3: junk is removed as whole words only: YOUTUBE is not
    /// YOU + TUBE.
    #[test]
    fn a_word_that_starts_with_a_junk_word_is_kept_whole() {
        assert_eq!(keyword_from_text("YOUTUBE ΣΕΙΣΜΟΣ").as_deref(), Some("YOUTUBESEISMOS"));
        assert_eq!(keyword_from_text("YouTube Shorts: σεισμός στην Κρήτη").as_deref(), Some("YOUTUBESEISMOS"));
        assert_eq!(keyword_from_text("Hasta la vista").as_deref(), Some("HASTAVISTA"));
        // Written apart ("You Tube", "you-tube" in a slug), it is still one name.
        assert_eq!(keyword_from_text("You Tube σεισμός").as_deref(), Some("YOUTUBESEISMOS"));
        assert_eq!(keyword_from_url("https://news.example/you-tube-seismos").as_deref(), Some("YOUTUBESEISMOS"));
        let r = roster();
        for subject in ["YOUTUBE ΣΕΙΣΜΟΣ", "YouTube Shorts σεισμός"] {
            let p = parse(&mail(subject, "a.papadaki@example.gr", "https://www.youtube.com/watch?v=abcdefghijk"), &r, &ParserConfig::default());
            let k = p.sections[0].jobs[0].keyword.clone();
            assert!(!k.starts_with("TUBE"), "{subject}: {k}");
        }
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
    // A keyword that is junk through and through (MP4, FILE382, a phone
    // number, HASSENT) names no story, from the model either (P15.10).
    (RE_KEYWORD.is_match(&k) && !STOPWORDS.contains(&k.as_str()) && !NOTHING.contains(&k.as_str()) && !is_junk_keyword(&k))
        .then_some(k)
}

/// A video keyword as the LLM wrote it (P4.34), in Greek or Latin, one or
/// two words: transliterated by this crate's ELOT 743 (so ΟΦΗ is OFI here
/// as everywhere else, not the model's own "OFY"), spaces and punctuation
/// dropped, at most 20 characters like every keyword, and refused when it
/// is a placeholder or names nothing (TRAILER).
pub fn keyword_from_model(raw: &str) -> Option<String> {
    let joined: String = translit(raw.trim()).chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let k: String = fold_stretched(&joined.to_ascii_uppercase()).chars().take(MAX_KEYWORD).collect();
    valid_keyword(&k).filter(|k| !is_generic_keyword(k))
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