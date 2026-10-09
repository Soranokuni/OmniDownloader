//! Which group a mail is for (plan P4.18): the news desk, a show, a desk.
//!
//! Deterministic, like the parser: the same mail, roster and groups always
//! give the same answer. In order:
//!
//! 1. a group named in the **subject** (its keywords, its name or its code);
//! 2. a group named in the **body** after "ΓΙΑ" ("για την πρωινή εκπομπή",
//!    "για το δελτίο"): a bare mention elsewhere is not routing, and
//!    "δελτίο τύπου" is a press release, not the news;
//! 3. the resolved journalist's **default group**.
//!
//! Several groups named at one step: the one the journalist belongs to wins;
//! otherwise it is ambiguous and the next step decides. Nothing decides:
//! no group (the LLM may be asked to choose from the list, plan P4.19).
//!
//! The result is a label on the job. It never changes a file name or where
//! the file is delivered.

use serde::{Deserialize, Serialize};

use omni_core::models::Journalist;
use omni_core::taxonomy::Group;
use omni_core::translit::translit;

use crate::mail::InboundMail;
use crate::parser::{self, warnings, ParsedEmail, Warning};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupResolution {
    Subject,
    Body,
    /// The journalist's default group.
    Member,
    /// Chosen by the LLM from the list (plan P4.19).
    LlmAssist,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedGroup {
    pub code: String,
    pub how: GroupResolution,
}

/// How many words after "ΓΙΑ" a group may be named in the body:
/// "για την πρωινή εκπομπή" puts the name three words on.
const BODY_REACH: usize = 4;

fn words(s: &str) -> Vec<String> {
    translit(s)
        .to_uppercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Each phrase that names `g`, as words.
fn phrases(g: &Group) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = g.keywords.iter().map(|k| words(k)).collect();
    out.push(words(&g.name));
    out.push(vec![g.code.clone()]);
    out.retain(|p| !p.is_empty());
    out
}

fn phrase_at(hay: &[String], at: usize, phrase: &[String]) -> bool {
    hay.len() >= at + phrase.len() && hay[at..at + phrase.len()] == *phrase
}

fn named_in_subject(subject: &[String], g: &Group) -> bool {
    phrases(g).iter().any(|p| (0..subject.len()).any(|i| phrase_at(subject, i, p)))
}

fn named_in_body(lines: &[Vec<String>], g: &Group) -> bool {
    let ps = phrases(g);
    lines.iter().any(|line| {
        line.iter().enumerate().filter(|(_, w)| *w == "GIA").any(|(i, _)| {
            (i + 1..=i + BODY_REACH).any(|at| ps.iter().any(|p| phrase_at(line, at, p)))
        })
    })
}

/// Pick one of `candidates`: the only one, or the only one the journalist
/// belongs to. `Err(())` when several remain.
fn pick(candidates: Vec<&Group>, member_of: &[String]) -> Result<Option<String>, ()> {
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(Some(candidates[0].code.clone())),
        _ => {
            let mine: Vec<&&Group> = candidates.iter().filter(|g| member_of.contains(&g.code)).collect();
            if mine.len() == 1 {
                Ok(Some(mine[0].code.clone()))
            } else {
                Err(())
            }
        }
    }
}

/// Decide the group and record it on `parsed` (with a `GROUP_AMBIGUOUS`
/// warning when a step named several).
pub fn resolve_group(mail: &InboundMail, parsed: &mut ParsedEmail, roster: &[Journalist], groups: &[Group]) {
    parsed.group = None;
    if groups.is_empty() {
        return;
    }
    let member_of: Vec<String> = roster
        .iter()
        .find(|j| j.surname == parsed.journalist.surname && j.surname != "MCR")
        .map(|j| j.groups.clone())
        .unwrap_or_default();

    let subject = words(&mail.subject);
    let body: Vec<Vec<String>> = parser::cleaned_body(mail).lines().map(words).collect();
    let steps: [(GroupResolution, Vec<&Group>); 2] = [
        (GroupResolution::Subject, groups.iter().filter(|g| named_in_subject(&subject, g)).collect()),
        (GroupResolution::Body, groups.iter().filter(|g| named_in_body(&body, g)).collect()),
    ];
    for (how, candidates) in steps {
        let listed: Vec<String> = candidates.iter().map(|g| g.code.clone()).collect();
        match pick(candidates, &member_of) {
            Ok(Some(code)) => {
                parsed.group = Some(ResolvedGroup { code, how });
                return;
            }
            Ok(None) => {}
            Err(()) => parsed.warnings.push(Warning {
                code: warnings::GROUP_AMBIGUOUS.into(),
                detail: Some(listed.join(", ")),
            }),
        }
    }
    if let Some(code) = member_of.first().filter(|c| groups.iter().any(|g| &g.code == *c)) {
        parsed.group = Some(ResolvedGroup {
            code: code.clone(),
            how: GroupResolution::Member,
        });
    }
}
