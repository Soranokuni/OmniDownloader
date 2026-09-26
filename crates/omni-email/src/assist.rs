//! LLM assist (plan P4.4): a second opinion where the parser is unsure.
//!
//! The deterministic parser decides what goes to air. The model is asked only
//! when the parser could not name the journalist of a mail that looks routed
//! to someone, could not make a keyword from a section title, or (plan P4.19)
//! the rules could not tell which group the mail is for. It may propose
//! exactly three things — a journalist, keywords, a group — and each proposal
//! is checked before use:
//!
//! * the journalist must resolve against the roster by the parser's own rules;
//! * a keyword must be `^[A-Z0-9]{2,20}$` after transliteration, for a section
//!   the parser actually produced;
//! * a group must be one of the codes it was shown.
//!
//! It never supplies URLs or indices. With the LLM down, slow or wrong, the
//! parser's result stands unchanged and the jobs are the same.

use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;
use std::time::Duration;
use tracing::{info, warn};

use omni_core::config::{LlmConfig, LlmMode, ParserConfig};
use omni_core::models::Journalist;
use omni_core::taxonomy::Group;
use omni_core::translit::translit;

use crate::llm::LlmClient;
use crate::mail::InboundMail;
use crate::groups::{resolve_group, GroupResolution, ResolvedGroup};
use crate::parser::{self, warnings, ParsedEmail, Resolution, Warning};

/// Most of a long rundown is irrelevant to "who is this for"; the model gets
/// the start of the body, which is where routing phrases are written.
const MAX_BODY_CHARS: usize = 4000;

static RE_ROUTING_PHRASE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(GIA|STO\s+ONOMA)\b").unwrap());

const SYSTEM_PROMPT: &str = "You help a Greek television newsroom file video links. \
Answer with one JSON object and nothing else: \
{\"journalist_surname_latin\": string or null, \"keywords\": {\"<section number>\": \"<KEYWORD>\"}, \
\"group_code\": string or null}. \
journalist_surname_latin: the surname of the journalist the email says the material is for, \
chosen from the roster, in uppercase Latin letters exactly as the roster writes it; null if the \
email does not say or you are unsure. keywords: for each section listed, one or two distinctive \
words of its title transliterated to uppercase Latin, letters and digits only, no spaces, at most \
20 characters. group_code: the code of the listed group (news desk or show) the material is for, \
judged from the subject, the body and the group descriptions; null if the email does not make it \
clear or no groups are listed. Never output URLs, section numbers or group codes that were not \
listed, or any other field.";

pub struct Assist {
    client: LlmClient,
    cfg: LlmConfig,
}

impl Assist {
    pub fn new(endpoint: &str, model: &str, cfg: LlmConfig) -> Self {
        if cfg.mode == LlmMode::Primary {
            warn!("llm.mode = \"primary\" is not implemented; running as \"assist\"");
        }
        let client = LlmClient::with_timeout(endpoint, model, Duration::from_secs(cfg.timeout_secs.clamp(5, 300)));
        Self { client, cfg }
    }

    /// Whether this mail is worth a model call (plan P4.4 trigger rules;
    /// P4.19: also when groups exist and the rules chose none).
    pub fn wanted(&self, parsed: &ParsedEmail, body: &str, groups: &[Group]) -> bool {
        if self.cfg.mode == LlmMode::Off || parsed.jobs().next().is_none() {
            return false;
        }
        let routed_but_unresolved = parsed.journalist.how == Resolution::Unresolved
            && RE_ROUTING_PHRASE.is_match(&translit(body));
        let group_undecided = parsed.group.is_none() && !groups.is_empty();
        self.cfg.keyword_polish
            || routed_but_unresolved
            || group_undecided
            || !parser::sections_needing_keyword(parsed).is_empty()
    }

    /// Ask, validate, apply. Never fails: a problem is recorded as a
    /// `LLM_ASSIST_SKIPPED` warning and the parser's result is kept.
    pub async fn refine(
        &self,
        mail: &InboundMail,
        body: &str,
        parsed: &mut ParsedEmail,
        roster: &[Journalist],
        groups: &[Group],
        parser_cfg: &ParserConfig,
    ) {
        let want_journalist = parsed.journalist.how == Resolution::Unresolved;
        let want_group = parsed.group.is_none() && !groups.is_empty();
        let sections: Vec<(String, String)> = if self.cfg.keyword_polish {
            parsed
                .sections
                .iter()
                .filter(|s| !s.jobs.is_empty())
                .filter_map(|s| s.title.clone().map(|t| (s.index_str.clone(), t)))
                .collect()
        } else {
            parser::sections_needing_keyword(parsed)
        };
        if !want_journalist && !want_group && sections.is_empty() {
            return;
        }

        let user = build_prompt(mail, body, roster, want_journalist, &sections, if want_group { groups } else { &[] });
        let answer = match self.client.chat_json(SYSTEM_PROMPT, &user, &schema(), self.cfg.max_tokens).await {
            Ok(v) => v,
            Err(e) => {
                warn!("LLM assist unavailable, keeping the parser's result: {e:#}");
                skipped(parsed, "unavailable");
                return;
            }
        };

        let mut applied = Vec::new();
        let mut rejected = Vec::new();

        if want_journalist {
            match answer.get("journalist_surname_latin") {
                Some(Value::String(name)) if !name.trim().is_empty() => {
                    match parser::resolve_name(roster, name) {
                        Some(surname) => {
                            info!("LLM assist: journalist {surname} for '{}'", mail.subject);
                            parser::adopt_journalist(parsed, surname.clone(), parser_cfg);
                            applied.push(format!("journalist {surname}"));
                        }
                        None => rejected.push("journalist not on the roster".to_string()),
                    }
                }
                _ => {}
            }
        }

        if want_group {
            match answer.get("group_code") {
                Some(Value::String(code)) if !code.trim().is_empty() => {
                    let code = code.trim().to_uppercase();
                    if groups.iter().any(|g| g.code == code) {
                        info!("LLM assist: group {code} for '{}'", mail.subject);
                        parsed.group = Some(ResolvedGroup {
                            code: code.clone(),
                            how: GroupResolution::LlmAssist,
                        });
                        applied.push(format!("group {code}"));
                    } else {
                        rejected.push(format!("group {code} is not in the list"));
                    }
                }
                _ => {}
            }
        }

        if let Some(Value::Object(map)) = answer.get("keywords") {
            let wanted: Vec<&str> = sections.iter().map(|(i, _)| i.as_str()).collect();
            let mut set = Vec::new();
            for (index, kw) in map {
                if !wanted.contains(&index.as_str()) {
                    rejected.push(format!("section {index} was not asked about"));
                    continue;
                }
                let Some(k) = kw.as_str().and_then(parser::valid_keyword) else {
                    rejected.push(format!("keyword for section {index} is not [A-Z0-9]{{2,20}}"));
                    continue;
                };
                if parser::set_section_keyword(parsed, index, &k, self.cfg.keyword_polish) {
                    set.push(format!("{index}={k}"));
                }
            }
            if !set.is_empty() {
                set.sort();
                applied.push(format!("keywords {}", set.join(" ")));
            }
        }

        if !rejected.is_empty() {
            warn!("LLM assist: discarded {}", rejected.join("; "));
        }
        if applied.is_empty() {
            skipped(parsed, if rejected.is_empty() { "nothing proposed" } else { "proposal rejected" });
        } else {
            parsed
                .warnings
                .push(Warning { code: warnings::LLM_ASSIST_APPLIED.into(), detail: Some(applied.join(", ")) });
        }
    }
}

fn skipped(parsed: &mut ParsedEmail, why: &str) {
    parsed
        .warnings
        .push(Warning { code: warnings::LLM_ASSIST_SKIPPED.into(), detail: Some(why.into()) });
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "journalist_surname_latin": { "type": ["string", "null"] },
            "keywords": { "type": "object", "additionalProperties": { "type": "string" } },
            "group_code": { "type": ["string", "null"] }
        },
        "required": ["journalist_surname_latin", "keywords", "group_code"],
        "additionalProperties": false
    })
}

fn build_prompt(
    mail: &InboundMail,
    body: &str,
    roster: &[Journalist],
    want_journalist: bool,
    sections: &[(String, String)],
    groups: &[Group],
) -> String {
    let mut p = String::new();
    if want_journalist {
        p.push_str("Roster (surname: full name):\n");
        for j in roster.iter().filter(|j| j.surname != "MCR") {
            p.push_str(&format!("- {}: {}\n", j.surname, j.full_name));
        }
    } else {
        p.push_str("The journalist is known; answer null for journalist_surname_latin.\n");
    }
    if sections.is_empty() {
        p.push_str("\nNo keywords are needed; answer {} for keywords.\n");
    } else {
        p.push_str("\nSections that need a keyword (number: title):\n");
        for (i, t) in sections {
            p.push_str(&format!("{i}: {t}\n"));
        }
    }
    if groups.is_empty() {
        p.push_str("\nNo group is needed; answer null for group_code.\n");
    } else {
        p.push_str("\nGroups (code: name, kind; what goes there):\n");
        for g in groups {
            p.push_str(&format!("- {}: {}, {}", g.code, g.name, g.kind));
            if !g.description.is_empty() {
                p.push_str(&format!("; {}", g.description));
            }
            p.push('\n');
        }
    }
    p.push_str(&format!("\nSubject: {}\nFrom: {} <{}>\n\nBody:\n", mail.subject, mail.from_name, mail.from_address));
    p.extend(body.chars().take(MAX_BODY_CHARS));
    p
}

/// Everything the daemon decides about a mail, in order (plan P4.18/P4.19):
/// parse, choose the group by rules, ask the assist where something is
/// still open, then apply the rules again. A journalist the assist named
/// may have a default group, and a rule outranks the model's guess.
pub async fn interpret(
    mail: &InboundMail,
    roster: &[Journalist],
    groups: &[Group],
    parser_cfg: &ParserConfig,
    assist: Option<&Assist>,
) -> ParsedEmail {
    let mut parsed = parser::parse(mail, roster, parser_cfg);
    resolve_group(mail, &mut parsed, roster, groups);
    if let Some(a) = assist {
        let body = mail.readable_body();
        if a.wanted(&parsed, &body, groups) {
            a.refine(mail, &body, &mut parsed, roster, groups, parser_cfg).await;
            let from_model = parsed.group.clone();
            resolve_group(mail, &mut parsed, roster, groups);
            if parsed.group.is_none() {
                parsed.group = from_model;
            }
        }
    }
    parsed
}
