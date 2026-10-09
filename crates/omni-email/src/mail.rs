//! The provider-neutral shape of one inbound email (plan P4.1).
//!
//! Every mail source reduces a message to [`InboundMail`]; the parser only
//! ever sees this type, which is what lets it be tested offline from `.eml`
//! fixtures.

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use mailparse::{parse_mail, DispositionType, MailHeaderMap, ParsedMail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// One attachment, described but not downloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMeta {
    /// Provider id: the Graph attachment id, or the MIME part path (`"2"`,
    /// `"1.3"`) for a raw RFC 822 message.
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboundMail {
    /// Provider id used to mark / move / reply (the Graph message id).
    pub id: String,
    /// RFC 5322 `Message-ID`, the idempotency key (E-07). Kept with its angle
    /// brackets exactly as the header carried them.
    pub internet_message_id: String,
    pub from_address: String,
    pub from_name: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub received_at: Option<DateTime<Utc>>,
    /// Plain-text body. Empty when the message only had HTML.
    pub body_text: String,
    pub body_html: Option<String>,
    pub attachments: Vec<AttachmentMeta>,
}

impl InboundMail {
    /// The body the parser should read: the text part, or the HTML part
    /// reduced to text when there is no usable text part (defect E-06).
    pub fn readable_body(&self) -> String {
        if !self.body_text.trim().is_empty() {
            return self.body_text.clone();
        }
        self.body_html.as_deref().map(html_to_text).unwrap_or_default()
    }

    /// Read a raw RFC 822 message (an `.eml` file or a fixture).
    pub fn from_rfc822(id: &str, raw: &[u8]) -> Result<Self> {
        let parsed = parse_mail(raw).context("Failed parsing RFC822 MIME message")?;
        let headers = parsed.get_headers();

        let (from_address, from_name) = headers
            .get_first_value("From")
            .and_then(|v| first_address(&v))
            .unwrap_or_default();
        let to = headers
            .get_first_value("To")
            .map(|v| all_addresses(&v))
            .unwrap_or_default();
        let cc = headers
            .get_first_value("Cc")
            .map(|v| all_addresses(&v))
            .unwrap_or_default();
        let received_at = headers
            .get_first_value("Date")
            .and_then(|d| mailparse::dateparse(&d).ok())
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single());

        let mut mail = InboundMail {
            id: id.to_string(),
            internet_message_id: headers
                .get_first_value("Message-ID")
                .unwrap_or_default()
                .trim()
                .to_string(),
            from_address,
            from_name,
            to,
            cc,
            subject: headers.get_first_value("Subject").unwrap_or_default().trim().to_string(),
            received_at,
            ..Default::default()
        };
        walk(&parsed, "", &mut mail);
        Ok(mail)
    }
}

fn first_address(value: &str) -> Option<(String, String)> {
    let list = mailparse::addrparse(value).ok()?;
    for addr in list.iter() {
        match addr {
            mailparse::MailAddr::Single(s) => {
                return Some((s.addr.trim().to_lowercase(), s.display_name.clone().unwrap_or_default()))
            }
            mailparse::MailAddr::Group(g) => {
                if let Some(s) = g.addrs.first() {
                    return Some((s.addr.trim().to_lowercase(), s.display_name.clone().unwrap_or_default()));
                }
            }
        }
    }
    None
}

fn all_addresses(value: &str) -> Vec<String> {
    let Ok(list) = mailparse::addrparse(value) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for addr in list.iter() {
        match addr {
            mailparse::MailAddr::Single(s) => out.push(s.addr.trim().to_lowercase()),
            mailparse::MailAddr::Group(g) => out.extend(g.addrs.iter().map(|s| s.addr.trim().to_lowercase())),
        }
    }
    out
}

/// Depth-first over the MIME tree: the first text/plain and text/html parts
/// that are not attachments become the body, everything with a filename or an
/// `attachment` disposition becomes an [`AttachmentMeta`].
///
/// The old reader looked only one level down, so the usual Outlook shape
/// (`multipart/mixed` → `multipart/alternative` → `text/plain`) produced an
/// empty body whenever the mail carried an attachment.
fn walk(part: &ParsedMail, path: &str, mail: &mut InboundMail) {
    if !part.subparts.is_empty() {
        for (i, sub) in part.subparts.iter().enumerate() {
            let child = if path.is_empty() {
                format!("{}", i + 1)
            } else {
                format!("{path}.{}", i + 1)
            };
            walk(sub, &child, mail);
        }
        return;
    }

    let mimetype = part.ctype.mimetype.to_ascii_lowercase();
    let disposition = part.get_content_disposition();
    let filename = disposition
        .params
        .get("filename")
        .or_else(|| part.ctype.params.get("name"))
        .cloned();
    let is_attachment = disposition.disposition == DispositionType::Attachment || filename.is_some();

    if is_attachment {
        let size = part.get_body_raw().map(|b| b.len() as u64).unwrap_or(0);
        mail.attachments.push(AttachmentMeta {
            id: if path.is_empty() { "1".into() } else { path.to_string() },
            name: filename.unwrap_or_else(|| "attachment".into()),
            content_type: mimetype,
            size,
        });
        return;
    }

    if mimetype == "text/plain" && mail.body_text.is_empty() {
        mail.body_text = decode_part(part);
    } else if mimetype == "text/html" && mail.body_html.is_none() {
        mail.body_html = Some(decode_part(part));
    }
}

fn decode_part(part: &ParsedMail) -> String {
    let bytes = part.get_body_raw().unwrap_or_default();
    decode_text_with_charset(&bytes, &part.ctype.charset)
}

/// Decode a body in its declared charset. Greek mail arrives as UTF-8,
/// ISO-8859-7 and Windows-1253; an undeclared or `us-ascii` label that turns
/// out to contain 8-bit bytes is almost always UTF-8 in practice.
pub fn decode_text_with_charset(bytes: &[u8], charset: &str) -> String {
    let label = charset.trim().to_ascii_lowercase();
    if label.is_empty() || label == "us-ascii" || label == "ascii" {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    match encoding_rs::Encoding::for_label(label.as_bytes()) {
        Some(enc) => enc.decode(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

static RE_DROP_BLOCKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<(script|style|head)\b.*?</(script|style|head)\s*>").unwrap());
static RE_COMMENTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
static RE_ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|'([^']*)')[^>]*>(.*?)</a\s*>"#).unwrap()
});
static RE_BREAKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<br\s*/?>|<hr\b[^>]*>|</(p|div|li|tr|h[1-6]|table|blockquote|pre)\s*>|<(p|div|li|tr|h[1-6]|blockquote)\b[^>]*>").unwrap()
});
/// Table cells sit side by side: a space, or two cells' words run together.
static RE_CELLS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</t[dh]\s*>").unwrap());
/// Anchor text that is itself an address, often shortened by the client
/// (`youtube.com/watch?v=ab…`): the href replaces it, or the parser would see
/// a second, broken link.
static RE_DISPLAY_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(https?://|www\.)?[a-z0-9-]+(\.[a-z0-9-]+)+(/\S*)?[…]?$").unwrap());
static RE_LIST_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<(/?)(ol|ul|li)\b([^>]*)>").unwrap());
static RE_OL_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bstart\s*=\s*["']?(\d{1,4})"#).unwrap());

/// Write the numbers of `<ol>` items into the text, as a mail client shows
/// them, honouring `start="5"`. Outlook sends a numbered story list as
/// `<ol>`; without the numbers every item fell into one section and lost
/// the index the journalist gave it (and the numbering restarts the plain
/// text part shows after an interruption, "1. 2. 3. 4. … 1. 2. 3.", are not
/// there in the HTML).
fn number_ordered_lists(html: &str) -> String {
    // (ordered, next number) per open list.
    let mut stack: Vec<(bool, u32)> = Vec::new();
    let mut out = String::with_capacity(html.len());
    let mut last = 0;
    for c in RE_LIST_TAGS.captures_iter(html) {
        let m = c.get(0).unwrap();
        out.push_str(&html[last..m.end()]);
        last = m.end();
        let closing = !c[1].is_empty();
        match (c[2].to_ascii_lowercase().as_str(), closing) {
            ("ol", false) => {
                let start = RE_OL_START.captures(&c[3]).and_then(|s| s[1].parse().ok()).unwrap_or(1);
                stack.push((true, start));
            }
            ("ul", false) => stack.push((false, 0)),
            ("ol" | "ul", true) => {
                stack.pop();
            }
            ("li", false) => {
                if let Some((true, n)) = stack.last_mut() {
                    out.push_str(&format!("{n}. "));
                    *n += 1;
                }
            }
            _ => {}
        }
    }
    out.push_str(&html[last..]);
    out
}

static RE_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static RE_ENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z]{2,8});").unwrap());

/// Reduce an HTML body to text lines, keeping every link target.
///
/// Journalists paste links in Outlook, which turns them into anchors; when the
/// anchor text is not the URL itself (a headline, "εδώ"), the href is appended
/// so the parser still sees it.
pub fn html_to_text(html: &str) -> String {
    let s = RE_COMMENTS.replace_all(html, "");
    let s = RE_DROP_BLOCKS.replace_all(&s, "");
    let s = RE_ANCHOR.replace_all(&s, |c: &regex::Captures| {
        let href = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("");
        let href = decode_entities(href);
        let inner = c.get(3).map(|m| m.as_str()).unwrap_or("");
        let inner_text = decode_entities(&RE_TAGS.replace_all(inner, ""));
        let is_web = href.starts_with("http://") || href.starts_with("https://");
        let inner_trimmed = inner_text.trim();
        if !is_web || inner_text.contains(href.as_str()) {
            inner_text
        } else if RE_DISPLAY_URL.is_match(inner_trimmed) {
            href
        } else {
            format!("{inner_trimmed} {href}")
        }
    });
    let s = number_ordered_lists(&s);
    let s = RE_CELLS.replace_all(&s, " ");
    let s = RE_BREAKS.replace_all(&s, "\n");
    let s = RE_TAGS.replace_all(&s, "");
    let s = decode_entities(&s);

    s.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_entities(s: &str) -> String {
    RE_ENTITY
        .replace_all(s, |c: &regex::Captures| {
            let name = &c[1];
            let decoded = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = name.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    "laquo" => Some('«'),
                    "raquo" => Some('»'),
                    "ndash" => Some('–'),
                    "mdash" => Some('—'),
                    _ => None,
                }
            };
            match decoded {
                // A non-breaking space must not glue a URL to the next word.
                Some('\u{a0}') => " ".to_string(),
                Some(ch) => ch.to_string(),
                None => c[0].to_string(),
            }
        })
        .into_owned()
        .replace('\u{a0}', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outlook on the web, 2026-10-05: a story list in two `<ol>`s around an
    /// unnumbered paragraph, the second continuing at 5.
    #[test]
    fn ordered_list_items_keep_their_numbers() {
        let html = r#"<ol start="1" data-x="{&quot;a&quot;:1}"><li><div><a href="https://a.example/x">https://a.example/x</a> + ΕΙΚΟΝΕΣ</div></li>
            <li><div>Τίτλος</div></li></ol><div><a href="https://b.example/y">https://b.example/y</a></div>
            <ol start="5"><li><div>https://c.example/z + ΠΛΑΝΑ</div></li><li>έκτο<ul><li>κουκκίδα</li></ul></li></ol>"#;
        let text = html_to_text(html);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec!["1.", "https://a.example/x + ΕΙΚΟΝΕΣ", "2.", "Τίτλος", "https://b.example/y", "5.", "https://c.example/z + ΠΛΑΝΑ", "6. έκτο", "κουκκίδα"],
            "{text}"
        );
    }

    #[test]
    fn a_shortened_display_address_is_replaced_by_its_href() {
        let html = r#"<p><a href="https://www.youtube.com/watch?v=abcdef123&amp;t=4">youtube.com/watch?v=abc…</a></p>
            <p><a href="https://youtu.be/q1">www.youtu.be/q1</a></p>
            <table><tr><td>ΚΕΛΙ</td><td>ΔΕΥΤΕΡΟ</td></tr></table>"#;
        let text = html_to_text(html);
        assert!(!text.contains("abc…"), "{text}");
        assert_eq!(text.lines().next(), Some("https://www.youtube.com/watch?v=abcdef123&t=4"), "{text}");
        assert!(text.contains("https://youtu.be/q1"), "{text}");
        assert!(!text.contains("www.youtu.be"), "{text}");
        assert!(text.contains("ΚΕΛΙ ΔΕΥΤΕΡΟ"), "{text}");
    }

    #[test]
    fn html_keeps_hrefs_behind_headline_anchors() {
        let html = r#"<html><head><style>p{color:red}</style></head><body>
            <p>1. &#928;&#945;&#961;&#941;λαση &amp; άλλα</p>
            <p><a href="https://www.youtube.com/watch?v=abc&amp;t=3">Δείτε το βίντεο</a></p>
            <div><a href="https://youtu.be/xyz">https://youtu.be/xyz</a></div>
            </body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("https://www.youtube.com/watch?v=abc&t=3"), "{text}");
        assert!(text.contains("Δείτε το βίντεο https://www.youtube.com"), "{text}");
        // An anchor whose text already is the URL is not doubled.
        assert_eq!(text.matches("https://youtu.be/xyz").count(), 1, "{text}");
        assert!(text.contains("1. Παρέλαση & άλλα"), "{text}");
        assert!(!text.contains("color:red"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn nested_multipart_body_is_found() {
        let raw = concat!(
            "From: Test <t@example.gr>\r\n",
            "Subject: x\r\n",
            "Message-ID: <a@b>\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"OUT\"\r\n\r\n",
            "--OUT\r\n",
            "Content-Type: multipart/alternative; boundary=\"IN\"\r\n\r\n",
            "--IN\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n\r\n",
            "https://youtu.be/abc\r\n",
            "--IN--\r\n",
            "--OUT\r\n",
            "Content-Type: video/mp4; name=\"clip.mp4\"\r\n",
            "Content-Disposition: attachment; filename=\"clip.mp4\"\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "AAAA\r\n",
            "--OUT--\r\n",
        );
        let mail = InboundMail::from_rfc822("1", raw.as_bytes()).unwrap();
        assert!(mail.body_text.contains("https://youtu.be/abc"));
        assert_eq!(mail.attachments.len(), 1);
        assert_eq!(mail.attachments[0].name, "clip.mp4");
        assert_eq!(mail.attachments[0].id, "2");
        assert_eq!(mail.from_address, "t@example.gr");
    }
}
