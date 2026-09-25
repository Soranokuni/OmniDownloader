//! Golden tests for the deterministic email parser (plan P4.3).
//!
//! Every `tests/fixtures/{name}.eml` is parsed against the roster below and
//! compared, field by field, with `{name}.expected.json`.
//!
//! Regenerating after an intended behaviour change:
//!
//! ```powershell
//! $env:OMNI_BLESS=1; cargo test -p omni-email --test golden_parser_tests; Remove-Item Env:OMNI_BLESS
//! ```
//!
//! then read the diff of every changed `.expected.json` before committing — a
//! blessed file is a claim about what goes to air.
//!
//! All names and addresses are invented (the repository is public).

use std::path::{Path, PathBuf};

use omni_core::models::Journalist;
use omni_email::mail::InboundMail;
use omni_email::parser::{parse, ParserConfig};

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
        j("PAPADAKI", "Anna Papadaki", &["a.papadaki@example.gr"], &["ΠΑΠΑΔΑΚΗ", "ΑΝΝΑ"]),
        j("NIKOLAOU", "Giorgos Nikolaou", &["g.nikolaou@example.gr"], &["ΝΙΚΟΛΑΟΥ", "ΓΙΩΡΓΟΣ"]),
        j("GEORGIOU", "Eleni Georgiou", &["e.georgiou@example.gr"], &["ΓΕΩΡΓΙΟΥ", "ΕΛΕΝΗ"]),
        j("DIMITRIOU", "Kostas Dimitriou", &["k.dimitriou@example.gr"], &["ΔΗΜΗΤΡΙΟΥ", "ΚΩΣΤΑ"]),
    ]
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

fn fixtures() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "eml"))
        .collect();
    v.sort();
    v
}

fn parse_fixture(eml: &Path) -> serde_json::Value {
    let raw = std::fs::read(eml).unwrap();
    let mail = InboundMail::from_rfc822("fixture", &raw).unwrap();
    let parsed = parse(&mail, &roster(), &ParserConfig::default());
    serde_json::to_value(&parsed).unwrap()
}

#[test]
fn every_fixture_matches_its_expected_json() {
    let bless = std::env::var_os("OMNI_BLESS").is_some();
    let mut failures = Vec::new();
    for eml in fixtures() {
        let got = parse_fixture(&eml);
        let expected_path = eml.with_extension("expected.json");
        if bless {
            let text = serde_json::to_string_pretty(&got).unwrap() + "\n";
            std::fs::write(&expected_path, text).unwrap();
            continue;
        }
        let expected: serde_json::Value = match std::fs::read_to_string(&expected_path) {
            Ok(t) => serde_json::from_str(&t).unwrap(),
            Err(_) => {
                failures.push(format!("{}: no .expected.json", eml.display()));
                continue;
            }
        };
        if got != expected {
            failures.push(format!(
                "{}\n--- expected\n{}\n--- got\n{}",
                eml.file_name().unwrap().to_string_lossy(),
                serde_json::to_string_pretty(&expected).unwrap(),
                serde_json::to_string_pretty(&got).unwrap()
            ));
        }
    }
    assert!(failures.is_empty(), "{} fixture(s) differ:\n\n{}", failures.len(), failures.join("\n\n"));
}

/// Plan P4.3 requires at least 15 cases; losing fixtures must not pass quietly.
#[test]
fn fixture_set_is_complete() {
    let names: Vec<String> = fixtures()
        .iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.len() >= 15, "only {} fixtures: {names:?}", names.len());
    for eml in fixtures() {
        assert!(eml.with_extension("expected.json").exists(), "{} has no expected.json", eml.display());
    }
}

/// The parser is a pure function: parsing twice gives the same answer.
#[test]
fn parsing_is_deterministic() {
    for eml in fixtures() {
        assert_eq!(parse_fixture(&eml), parse_fixture(&eml), "{}", eml.display());
    }
}
