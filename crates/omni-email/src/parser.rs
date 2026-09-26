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
    /// Nobody matched; the jobs go to `MCR`.
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
    Ambiguous,
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
            _ => Some(Lookup::Ambiguous),
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
}

static RE_OVERRIDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bSTO\s+ONOMA\s+(TIS|TOU|THS|MOU)\b(?:\s+([A-Z0-9]+))?(?:\s+([A-Z0-9]+))?").unwrap()
});
static RE_SUBJECT_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(RE|FW|FWD|AP|PRTH|SCHET|TR|WG)\s*:\s*").unwrap());
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

fn is_forward(subject: &str) -> bool {
    let s = translit(subject);
    let s = s.trim_start();
    ["FW:", "FWD:", "FW :", "PRTH:", "TR:", "WG:"].iter().any(|p| s.starts_with(p))
}

fn resolve_journalist(
    mail: &InboundMail,
    body: &str,
    roster: &Roster,
    warnings: &mut Vec<Warning>,
) -> ResolvedJournalist {
    let mut ambiguous = false;

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
                    Lookup::Ambiguous => ambiguous = true,
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
    if let Some(c) = RE_SUBJECT_NAME.captures(&subject) {
        for t in tokens(&c[2]) {
            match roster.lookup(t) {
                Lookup::One(s) => {
                    return ResolvedJournalist {
                        surname: s,
                        how: Resolution::Subject,
                    }
                }
                Lookup::Ambiguous => ambiguous = true,
                Lookup::None => {}
            }
        }
    }

    // c. Sender address.
    if let Some(s) = roster.by_sender(&mail.from_address) {
        return ResolvedJournalist {
            surname: s,
            how: Resolution::Sender,
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
static RE_WROTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(on|στις|την)\s.+(wrote|έγραψε|γράψατε)\s*:?\s*$").unwrap());
static RE_HEADER_FROM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\*?(from|από)\s*:").unwrap());
static RE_HEADER_ANY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\*?(from|sent|date|to|cc|subject|από|εστάλη|ημερομηνία|προς|κοιν|θέμα)\s*:").unwrap()
});

/// Remove quoted replies and signatures.
///
/// A reply quotes what the journalist already sent, so everything below the
/// reply header is dropped. A *forward* is different: the forwarded press
/// release or colleague's mail is the content, so only the header lines go.
fn strip_quotes_and_signature(body: &str, forward: bool) -> String {
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
static RE_NUMBERED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\d{1,3})(\s*)([.)\-:])?(\s*)(.*)$").unwrap());
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
        .filter_map(|m| clean_url(m.as_str()).map(|u| (m.range(), u)))
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
        let no_urls = strip_urls(&self.text);
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

/// Trim punctuation a sentence glued to the link, add the scheme to a bare
/// `www.` link, and unwrap Outlook Safe Links so the real target is queued.
fn clean_url(raw: &str) -> Option<String> {
    let mut u = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', '>', '»', '"', '\'']).to_string();
    // A closing parenthesis belongs to the URL only if it opened one.
    while u.ends_with(')') && u.matches(')').count() > u.matches('(').count() {
        u.pop();
        u = u.trim_end_matches(['.', ',', ';', ':', '!', '?']).to_string();
    }
    if u.to_ascii_lowercase().starts_with("www.") {
        u = format!("https://{u}");
    }
    let parsed = Url::parse(&u).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host.ends_with("safelinks.protection.outlook.com") {
        let target = parsed.query_pairs().find(|(k, _)| k == "url").map(|(_, v)| v.into_owned())?;
        return clean_url(&target);
    }
    Some(u)
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
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
    let rest = c[5].to_string();
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
    for line in lines {
        if let Some((n, rest)) = numbered(&line.text, expected) {
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
    LazyLock::new(|| Regex::new(r"(?i)^\s*(RE|FW|FWD|ΑΠ|ΠΡΘ|ΣΧΕΤ|TR|WG)\s*:\s*").unwrap());

/// Title and keyword from the subject of an unnumbered email: the title is
/// the subject as written (minus `RE:`/`FW:`), the keyword leaves out routing
/// words and the journalist's own name. `None` if nothing usable remains.
fn subject_title(subject: &str, roster: &Roster, journalist: &str) -> Option<(String, String)> {
    let latin = subject_without_prefixes(&translit(subject));
    let kept: Vec<&str> = tokens(&latin)
        .filter(|t| !matches!(roster.lookup(t), Lookup::One(ref s) if s == journalist))
        .filter(|t| !["THEMATA", "EPIKAIROTITA", "EPIKAIROTHTA", "GIA", "MONTAZ", "EKTAKTO", "BREAKING", "URGENT"].contains(t))
        .collect();
    let keyword = keyword_from_text(&kept.join(" "))?;
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

pub fn parse(mail: &InboundMail, roster: &[Journalist], cfg: &ParserConfig) -> ParsedEmail {
    let mut warnings = Vec::new();
    let roster_ix = Roster::new(roster);

    // 1. Cleanup. Zero-width and soft-hyphen characters come along when a
    // link is copied out of a chat app or a web page, and split the URL.
    let body = mail.readable_body().replace(INVISIBLE, "");
    let body = strip_quotes_and_signature(&body, is_forward(&mail.subject));
    let body = decontaminate_email_body(&body);

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
        for line in &s.lines {
            for u in &line.urls {
                if !seen_urls.insert(urlnorm::normalize(u)) {
                    continue;
                }
                candidates.push((u.clone(), classify(u, cfg), marked.contains(u)));
            }
        }

        let any_marked = candidates.iter().any(|(_, t, m)| *m && !matches!(t, Tier::Locker | Tier::Image));
        let has = |tier: Tier| candidates.iter().any(|(_, t, _)| *t == tier);
        let wanted = if any_marked {
            None
        } else if has(Tier::Tier1) {
            Some(Tier::Tier1)
        } else if has(Tier::Tier2) {
            Some(Tier::Tier2)
        } else {
            Some(Tier::Other)
        };

        let mut selected: Vec<(String, Tier, bool)> = Vec::new();
        for (u, tier, m) in candidates {
            let take = match tier {
                Tier::Locker => true,
                Tier::Image => false,
                _ => match wanted {
                    None => m,
                    Some(w) => tier == w,
                },
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
                let confidence = round2(base_confidence(tier, marker) - penalty);
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
        assert!(matches!(ix.lookup("ΑΝΝΑΣ"), Lookup::Ambiguous));
        assert!(matches!(ix.lookup("ΤΟ"), Lookup::None));
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
    (RE_KEYWORD.is_match(&k) && !STOPWORDS.contains(&k.as_str())).then_some(k)
}

/// Adopt a journalist for a mail the parser left unresolved: the −0.2
/// confidence penalty is taken back and every job's status recomputed.
pub fn adopt_journalist(parsed: &mut ParsedEmail, surname: String, cfg: &ParserConfig) {
    if parsed.journalist.how != Resolution::Unresolved {
        return;
    }
    parsed.journalist = ResolvedJournalist {
        surname,
        how: Resolution::LlmAssist,
    };
    parsed
        .warnings
        .retain(|w| w.code != warnings::JOURNALIST_UNRESOLVED && w.code != warnings::JOURNALIST_AMBIGUOUS);
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