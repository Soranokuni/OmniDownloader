//! The newsroom taxonomy (plan P4.17): groups (the news desk, each show)
//! and who belongs to which, on top of the journalist roster.
//!
//! A group is a **label**: it is stored on the job and shown in the panels,
//! and never changes a file name or where a file is delivered.
//!
//! `taxonomy.json` is the import/export form. The database is the source of
//! truth; the file is for backup, bulk edits, and a first seed
//! (`data/taxonomy.json`, gitignored: it holds real names).

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;

pub const TAXONOMY_VERSION: u32 = 1;

/// What kind of output a group is. Only a hint for people and the LLM.
pub const GROUP_KINDS: &[&str] = &["news", "show", "desk", "other"];

static RE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z0-9_]{2,20}$").unwrap());
static RE_SURNAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z0-9_]{2,40}$").unwrap());

/// One group, as stored and as written in taxonomy.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// `NEWS`, `SPORTS`, `KALIMERA`: uppercase Latin, digits, `_`.
    pub code: String,
    /// What people call it: "Κεντρικό Δελτίο Ειδήσεων".
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Words that name this group in a subject or body ("ΔΕΛΤΙΟ", "ΚΑΛΗΜΕΡΑ
    /// ΚΡΗΤΗ"). Compared accent- and case-insensitively, in Latin.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// One or two sentences for people and the LLM: what goes here.
    #[serde(default)]
    pub description: String,
}

fn default_kind() -> String {
    "show".into()
}

/// One person, as written in taxonomy.json. `groups[0]` is their default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub surname: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub emails: Vec<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub default_priority: i32,
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Taxonomy {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub people: Vec<Person>,
}

fn default_version() -> u32 {
    TAXONOMY_VERSION
}

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    pub groups_saved: usize,
    pub people_saved: usize,
    pub groups_removed: usize,
    pub people_removed: usize,
}

impl Group {
    /// Trim, uppercase the code, drop blank keywords.
    pub fn normalized(&self) -> Self {
        Self {
            code: self.code.trim().to_uppercase(),
            name: self.name.trim().to_string(),
            kind: self.kind.trim().to_lowercase(),
            keywords: clean_list(&self.keywords, 60),
            description: self.description.trim().chars().take(500).collect(),
        }
    }

    /// Every problem with this group; empty when it is valid.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if !RE_CODE.is_match(&self.code) {
            p.push(format!("group code `{}`: 2–20 of A–Z, 0–9, _", self.code));
        }
        if self.name.is_empty() {
            p.push(format!("group {}: a name is required", self.code));
        }
        if !GROUP_KINDS.contains(&self.kind.as_str()) {
            p.push(format!("group {}: kind must be one of {}", self.code, GROUP_KINDS.join(", ")));
        }
        if self.keywords.len() > 30 {
            p.push(format!("group {}: at most 30 keywords", self.code));
        }
        p
    }

    pub fn validate(&self) -> Result<(), String> {
        let p = self.problems();
        if p.is_empty() {
            Ok(())
        } else {
            Err(p.join("; "))
        }
    }
}

impl Person {
    pub fn normalized(&self) -> Self {
        let surname = self.surname.trim().to_uppercase();
        let full_name = if self.full_name.trim().is_empty() {
            surname.clone()
        } else {
            self.full_name.trim().to_string()
        };
        let mut seen = HashSet::new();
        Self {
            surname,
            full_name,
            emails: clean_list(&self.emails, 120).into_iter().map(|e| e.to_lowercase()).collect(),
            aliases: clean_list(&self.aliases, 40),
            default_priority: self.default_priority.clamp(-100, 100),
            groups: self
                .groups
                .iter()
                .map(|g| g.trim().to_uppercase())
                .filter(|g| !g.is_empty() && seen.insert(g.clone()))
                .collect(),
        }
    }

    /// Every problem with this person; empty when valid.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if !RE_SURNAME.is_match(&self.surname) {
            p.push(format!("person `{}`: surname must be Latin uppercase (as in slugs)", self.surname));
        }
        for e in self.emails.iter().filter(|e| !e.contains('@')) {
            p.push(format!("person {}: `{e}` is not an address", self.surname));
        }
        if self.emails.len() > 20 || self.aliases.len() > 30 {
            p.push(format!("person {}: at most 20 emails and 30 aliases", self.surname));
        }
        p
    }
}

impl Taxonomy {
    /// Normalise, then check everything, reporting every problem at once so
    /// a hand-edited file is fixed in one pass. `known_groups` are codes
    /// already in the database, which people may also refer to.
    pub fn checked(&self, known_groups: &[String]) -> Result<Taxonomy, Vec<String>> {
        let mut errors = Vec::new();
        if self.version != TAXONOMY_VERSION {
            errors.push(format!("version {} is not supported (expected {TAXONOMY_VERSION})", self.version));
        }
        let groups: Vec<Group> = self.groups.iter().map(Group::normalized).collect();
        let people: Vec<Person> = self.people.iter().map(Person::normalized).collect();

        let mut codes = HashSet::new();
        for g in &groups {
            errors.extend(g.problems());
            if !codes.insert(g.code.clone()) {
                errors.push(format!("group {} appears twice", g.code));
            }
        }
        let mut surnames = HashSet::new();
        let mut addresses = HashSet::new();
        for p in &people {
            errors.extend(p.problems());
            if !surnames.insert(p.surname.clone()) {
                errors.push(format!("person {} appears twice", p.surname));
            }
            for e in &p.emails {
                if !addresses.insert(e.clone()) {
                    errors.push(format!("address {e} belongs to two people"));
                }
            }
            for g in &p.groups {
                if !codes.contains(g) && !known_groups.contains(g) {
                    errors.push(format!("person {}: no group {g}", p.surname));
                }
            }
        }
        if errors.is_empty() {
            Ok(Taxonomy {
                version: TAXONOMY_VERSION,
                groups,
                people,
            })
        } else {
            Err(errors)
        }
    }
}

/// Trim, cap, drop blanks, and drop repeats — "δελτίο" repeats "ΔΕΛΤΙΟ":
/// the parser compares in accent-free Latin, so the list does too.
fn clean_list(v: &[String], max_len: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    v.iter()
        .map(|s| s.trim().chars().take(max_len).collect::<String>())
        .filter(|s| !s.is_empty() && seen.insert(crate::translit::translit(s).to_uppercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Taxonomy {
        serde_json::from_str(
            r#"{
              "version": 1,
              "groups": [
                {"code": "news", "name": "Δελτίο Ειδήσεων", "kind": "news", "keywords": ["ΔΕΛΤΙΟ", " ", "δελτίο"]},
                {"code": "KALIMERA", "name": "Καλημέρα", "description": "Πρωινή εκπομπή"}
              ],
              "people": [
                {"surname": "papadaki", "emails": ["A.Papadaki@example.gr"], "groups": ["NEWS", "kalimera", "NEWS"]}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_file_is_normalised_on_import() {
        let t = sample().checked(&[]).unwrap();
        assert_eq!(t.groups[0].code, "NEWS");
        assert_eq!(t.groups[0].keywords, vec!["ΔΕΛΤΙΟ"], "blank and duplicate keywords dropped");
        assert_eq!(t.groups[1].kind, "show", "kind defaults to show");
        let p = &t.people[0];
        assert_eq!(p.surname, "PAPADAKI");
        assert_eq!(p.full_name, "PAPADAKI");
        assert_eq!(p.emails, vec!["a.papadaki@example.gr"]);
        assert_eq!(p.groups, vec!["NEWS", "KALIMERA"], "order kept (first = default), duplicates dropped");
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let mut t = sample();
        t.groups.push(Group {
            code: "bad code".into(),
            name: "".into(),
            kind: "radio".into(),
            keywords: vec![],
            description: "".into(),
        });
        t.people.push(Person {
            surname: "Γεωργίου".into(),
            full_name: "".into(),
            emails: vec!["a.papadaki@example.gr".into(), "not-an-address".into()],
            aliases: vec![],
            default_priority: 0,
            groups: vec!["SPORTS".into()],
        });
        let errors = t.checked(&[]).unwrap_err();
        let all = errors.join("\n");
        for needle in ["BAD CODE", "a name is required", "kind must be", "Latin uppercase", "not an address", "belongs to two people", "no group SPORTS"] {
            assert!(all.contains(needle), "missing `{needle}` in:\n{all}");
        }
        // A group that already exists in the database may be referred to.
        let mut ok = sample();
        ok.people[0].groups.push("SPORTS".into());
        assert!(ok.checked(&["SPORTS".into()]).is_ok());
    }
}
