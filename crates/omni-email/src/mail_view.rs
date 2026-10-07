//! What the MCR mail view needs to know about a mail (plan P7.6, P7.7).
//!
//! The watcher stores a [`MailParseSummary`] with each handled mail, so the
//! desk can say who the mail was filed under and why, which sections it had
//! and what the parser warned about, without parsing the mail again (the
//! roster and the parser may have changed since).

use serde::{Deserialize, Serialize};

use crate::groups::ResolvedGroup;
use crate::parser::{Outcome, ParsedEmail, Resolution, Warning};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        // Greek letters are two bytes: an odd cut would split one.
        let text = "Α".repeat(MAX_STORED_TEXT);
        let kept = stored_text(&text);
        assert!(kept.len() <= MAX_STORED_TEXT + "\n…".len());
        assert!(kept.ends_with('…'));
        assert_eq!(stored_text("σύντομο"), "σύντομο");
    }
}
