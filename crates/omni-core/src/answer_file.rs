//! The answer file of an unattended install (P13.1b): the settings the admin
//! page exports (`GET /api/admin/settings/answer-file`), read back onto the
//! starting configuration of a new installation.
//!
//! Strict on purpose: a key this version does not know, a value of the wrong
//! type or anything that looks like a secret refuses the whole file, before
//! anything is installed. A secret is never in an answer file; the mailbox
//! secret and the LLM key are entered in the setup wizard.

use serde_json::{Map, Value};

use crate::config::{AppConfig, LlmMode};

/// The only answer-file format this version reads.
pub const ANSWER_FILE_VERSION: u64 = 1;

/// Name fragments of a key that would carry a secret. None of the exported
/// keys contains one (a test keeps it so).
const SECRET_NAMES: &[&str] =
    &["secret", "password", "passphrase", "passwd", "token", "api_key", "apikey", "private_key", "key_path", "cookie", "credential"];

/// The wizard modes `setup_mode` may hold.
const SETUP_MODES: &[&str] = &["beginner", "intermediate", "it"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnswerFileError {
    #[error("The answer file is not a JSON object.")]
    NotAnObject,
    #[error("The answer file has no answer_file_version; use a file exported from the admin page.")]
    NoVersion,
    #[error("The answer file has answer_file_version {found}; this Setup reads version 1.")]
    Version { found: String },
    #[error("`{key}` in the answer file is not a setting this version knows; remove it.")]
    Unknown { key: String },
    #[error("`{key}` in the answer file looks like a secret. An answer file never holds one: secrets are entered in the setup wizard.")]
    Secret { key: String },
    #[error("`{key}` in the answer file must be {expected}.")]
    WrongType { key: String, expected: &'static str },
    #[error("`{key}` in the answer file: \"{entry}\" is not a network. Use the form 10.20.0.0/16 (or 10.20.0.5/32 for one machine).")]
    BadNetwork { key: String, entry: String },
}

type R<T> = Result<T, AnswerFileError>;

fn wrong(key: &str, expected: &'static str) -> AnswerFileError {
    AnswerFileError::WrongType { key: key.to_string(), expected }
}

fn string(v: &Value, key: &str) -> R<String> {
    v.as_str().map(str::to_string).ok_or_else(|| wrong(key, "a string"))
}

fn boolean(v: &Value, key: &str) -> R<bool> {
    v.as_bool().ok_or_else(|| wrong(key, "true or false"))
}

fn integer(v: &Value, key: &str) -> R<i64> {
    v.as_i64().ok_or_else(|| wrong(key, "a whole number"))
}

fn strings(v: &Value, key: &str) -> R<Vec<String>> {
    let items = v.as_array().ok_or_else(|| wrong(key, "a list of strings"))?;
    items.iter().map(|i| i.as_str().map(str::to_string).ok_or_else(|| wrong(key, "a list of strings"))).collect()
}

fn optional_string(v: &Value, key: &str) -> R<Option<String>> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err(wrong(key, "a string or null")),
    }
}

fn object<'a>(v: &'a Value, key: &str) -> R<&'a Map<String, Value>> {
    v.as_object().ok_or_else(|| wrong(key, "an object"))
}

/// Refuse a key whose name looks like a secret, at any level, before its value is read.
fn refuse_secret_names(map: &Map<String, Value>, prefix: &str) -> R<()> {
    for (name, value) in map {
        let key = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let lower = name.to_ascii_lowercase();
        if SECRET_NAMES.iter().any(|s| lower.contains(s)) {
            return Err(AnswerFileError::Secret { key });
        }
        if let Value::Object(inner) = value {
            refuse_secret_names(inner, &key)?;
        }
    }
    Ok(())
}

/// The note when an answer file names a certificate.
pub const TLS_NOTE: &str =
    "TLS is not taken from an answer file: configure the certificate in the admin page after the install.";

/// An applied answer file: the configuration, and what Setup should tell the
/// operator about what it did not take.
#[derive(Debug, Clone)]
pub struct Applied {
    pub config: AppConfig,
    pub notes: Vec<String>,
}

/// A list of networks, each of which must parse as a CIDR block.
fn networks(v: &Value, key: &str) -> R<Vec<String>> {
    let list: Vec<String> = strings(v, key)?.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if let Err(bad) = crate::net::CidrSet::parse(&list) {
        let entry = bad.first().map(|(e, _)| e.clone()).unwrap_or_default();
        return Err(AnswerFileError::BadNetwork { key: key.to_string(), entry });
    }
    Ok(list)
}

/// The settings of `answers` applied onto `base`. Every key the admin page
/// exports is mapped; `watchfolder_resolved` (derived) is accepted and ignored;
/// `setup_completed` is accepted and never applied, because a new installation
/// always starts with the wizard pending (its first-run account still has to be
/// replaced and its secrets entered there). `tls.cert_path` is checked and not
/// applied: the certificate file and its passphrase are not on the new machine,
/// and a panel that cannot start would leave the wizard unreachable.
pub fn apply(answers: &Value, base: AppConfig) -> Result<Applied, AnswerFileError> {
    let doc = answers.as_object().ok_or(AnswerFileError::NotAnObject)?;
    refuse_secret_names(doc, "")?;
    match doc.get("answer_file_version") {
        None => return Err(AnswerFileError::NoVersion),
        Some(v) if v.as_u64() == Some(ANSWER_FILE_VERSION) => {}
        Some(v) => return Err(AnswerFileError::Version { found: v.to_string() }),
    }
    let mut notes = Vec::new();
    let mut c = base;
    for (name, v) in doc {
        let key = name.as_str();
        match key {
            "answer_file_version" => {}
            "setup_completed" => {
                boolean(v, key)?;
            }
            "watchfolder_resolved" => {
                string(v, key)?;
            }
            "setup_mode" => {
                let mode = string(v, key)?;
                if !SETUP_MODES.contains(&mode.as_str()) {
                    return Err(wrong(key, "beginner, intermediate or it"));
                }
                c.setup_mode = mode;
            }
            "watchfolder_path" => c.watchfolder_path = string(v, key)?,
            "web_port" => {
                c.web_port = v
                    .as_u64()
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|p| *p != 0)
                    .ok_or_else(|| wrong(key, "a port number from 1 to 65535"))?;
            }
            "mcr_open_networks" => c.security.mcr_open_networks = networks(v, key)?,
            "trust_proxy_header" => c.security.trust_proxy_header = boolean(v, key)?,
            "trusted_proxies" => c.security.trusted_proxies = networks(v, key)?,
            "allowed_hosts" => c.security.allowed_hosts = strings(v, key)?,
            "archive" => {
                for (n, v) in object(v, key)? {
                    let k = format!("archive.{n}");
                    match n.as_str() {
                        "enabled" => c.archive.enabled = boolean(v, &k)?,
                        "retention_days" => {
                            // The admin page's bounds; a huge value would overflow the nightly purge's date arithmetic.
                            let days = integer(v, &k)?;
                            if !(1..=3650).contains(&days) {
                                return Err(wrong(&k, "a number of days from 1 to 3650"));
                            }
                            c.archive.retention_days = days;
                        }
                        _ => return Err(AnswerFileError::Unknown { key: k }),
                    }
                }
            }
            "tls" => {
                for (n, v) in object(v, key)? {
                    let k = format!("tls.{n}");
                    match n.as_str() {
                        "cert_path" => {
                            if optional_string(v, &k)?.is_some() {
                                notes.push(TLS_NOTE.to_string());
                            }
                        }
                        _ => return Err(AnswerFileError::Unknown { key: k }),
                    }
                }
            }
            "metrics" => {
                for (n, v) in object(v, key)? {
                    let k = format!("metrics.{n}");
                    match n.as_str() {
                        "enabled" => c.metrics.enabled = boolean(v, &k)?,
                        "allow_networks" => c.metrics.allow_networks = networks(v, &k)?,
                        _ => return Err(AnswerFileError::Unknown { key: k }),
                    }
                }
            }
            "graph" => {
                for (n, v) in object(v, key)? {
                    let k = format!("graph.{n}");
                    match n.as_str() {
                        "tenant_id" => c.graph.tenant_id = string(v, &k)?,
                        "client_id" => c.graph.client_id = string(v, &k)?,
                        "mailbox" => c.graph.mailbox = string(v, &k)?,
                        _ => return Err(AnswerFileError::Unknown { key: k }),
                    }
                }
            }
            "llm" => {
                for (n, v) in object(v, key)? {
                    let k = format!("llm.{n}");
                    match n.as_str() {
                        "mode" => {
                            c.llm.mode = serde_json::from_value::<LlmMode>(v.clone())
                                .map_err(|_| wrong(&k, "off, assist or primary"))?
                        }
                        "provider" => c.llm.provider = string(v, &k)?,
                        "endpoint" => c.ollama_endpoint = string(v, &k)?,
                        "model" => c.ollama_model = string(v, &k)?,
                        _ => return Err(AnswerFileError::Unknown { key: k }),
                    }
                }
            }
            _ => return Err(AnswerFileError::Unknown { key: key.to_string() }),
        }
    }
    Ok(Applied { config: c, notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full() -> Value {
        json!({
            "answer_file_version": 1,
            "setup_completed": true,
            "setup_mode": "intermediate",
            "watchfolder_path": r"\\dalet\ingest",
            "watchfolder_resolved": r"\\dalet\ingest",
            "web_port": 8091,
            "mcr_open_networks": ["10.20.0.0/16"],
            "archive": { "enabled": false, "retention_days": 30 },
            "tls": { "cert_path": r"C:\certs\panel.pfx" },
            "trust_proxy_header": true,
            "trusted_proxies": ["10.0.0.5/32"],
            "allowed_hosts": ["ingest.newsroom.example"],
            "metrics": { "enabled": true, "allow_networks": ["10.9.0.0/24"] },
            "graph": { "tenant_id": "tenant-x", "client_id": "client-x", "mailbox": "ingest@newsroom.example" },
            "llm": { "mode": "off", "provider": "openai", "endpoint": "http://llm.example/v1", "model": "model-x" },
        })
    }

    #[test]
    fn answer_file_every_exported_key_is_applied_and_setup_completed_never_is() {
        let applied = apply(&full(), AppConfig::default()).unwrap();
        let c = applied.config;
        assert!(!c.setup_completed, "a new installation always starts with the wizard pending");
        assert_eq!(c.setup_mode, "intermediate");
        assert_eq!(c.watchfolder_path, r"\\dalet\ingest");
        assert_eq!(c.web_port, 8091);
        assert_eq!(c.security.mcr_open_networks, ["10.20.0.0/16"]);
        assert!(!c.archive.enabled);
        assert_eq!(c.archive.retention_days, 30);
        assert!(c.security.trust_proxy_header);
        assert_eq!(c.security.trusted_proxies, ["10.0.0.5/32"]);
        assert_eq!(c.security.allowed_hosts, ["ingest.newsroom.example"]);
        assert!(c.metrics.enabled);
        assert_eq!(c.metrics.allow_networks, ["10.9.0.0/24"]);
        assert_eq!((c.graph.tenant_id.as_str(), c.graph.client_id.as_str()), ("tenant-x", "client-x"));
        assert_eq!(c.graph.mailbox, "ingest@newsroom.example");
        assert_eq!(c.llm.mode, LlmMode::Off);
        assert_eq!(c.llm.provider, "openai");
        assert_eq!(c.ollama_endpoint, "http://llm.example/v1");
        assert_eq!(c.ollama_model, "model-x");
        // What it does not name keeps the base's value.
        assert_eq!(c.temp_path, AppConfig::default().temp_path);
    }

    /// R8: a certificate named in the file is not on the new machine (nor its
    /// passphrase): applied, the panel would not start and the wizard would be unreachable.
    #[test]
    fn answer_file_tls_is_checked_noted_and_never_applied() {
        let applied = apply(&full(), AppConfig::default()).unwrap();
        assert_eq!(applied.config.tls.cert_path, None);
        assert_eq!(applied.notes, [TLS_NOTE]);
        let mut doc = full();
        doc["tls"]["cert_path"] = Value::Null;
        let applied = apply(&doc, AppConfig::default()).unwrap();
        assert!(applied.notes.is_empty() && applied.config.tls.cert_path.is_none());
        doc["tls"]["cert_path"] = json!(5);
        assert!(matches!(apply(&doc, AppConfig::default()), Err(AnswerFileError::WrongType { key, .. }) if key == "tls.cert_path"));
    }

    #[test]
    fn answer_file_retention_days_and_the_port_stay_in_range() {
        for days in [0, -1, 3651, i64::MAX] {
            let mut doc = full();
            doc["archive"]["retention_days"] = json!(days);
            assert!(
                matches!(apply(&doc, AppConfig::default()), Err(AnswerFileError::WrongType { ref key, .. }) if key == "archive.retention_days"),
                "{days}"
            );
        }
        for days in [1, 3650] {
            let mut doc = full();
            doc["archive"]["retention_days"] = json!(days);
            assert_eq!(apply(&doc, AppConfig::default()).unwrap().config.archive.retention_days, days);
        }
        for port in [json!(1), json!(65535)] {
            let mut doc = full();
            doc["web_port"] = port.clone();
            assert_eq!(apply(&doc, AppConfig::default()).unwrap().config.web_port as u64, port.as_u64().unwrap());
        }
    }

    #[test]
    fn answer_file_every_network_list_must_hold_networks() {
        for (path, key) in [(None, "mcr_open_networks"), (None, "trusted_proxies"), (Some("metrics"), "allow_networks")] {
            let mut doc = full();
            let bad = json!(["10.0.0.0/8", "newsroom-lan"]);
            match path {
                None => doc[key] = bad,
                Some(s) => doc[s][key] = bad,
            }
            let want = path.map_or(key.to_string(), |s| format!("{s}.{key}"));
            let err = apply(&doc, AppConfig::default()).unwrap_err();
            assert_eq!(err, AnswerFileError::BadNetwork { key: want.clone(), entry: "newsroom-lan".into() });
            assert!(err.to_string().contains(&want) && err.to_string().contains("newsroom-lan"), "{err}");
        }
        // Host names are not networks.
        let mut doc = full();
        doc["allowed_hosts"] = json!(["ingest.newsroom.example", "mcr"]);
        assert!(apply(&doc, AppConfig::default()).is_ok());
    }

    #[test]
    fn answer_file_version_must_be_exactly_one() {
        let mut doc = full();
        doc.as_object_mut().unwrap().remove("answer_file_version");
        assert_eq!(apply(&doc, AppConfig::default()).unwrap_err(), AnswerFileError::NoVersion);
        for v in [json!(2), json!("1"), json!(1.5), json!(0)] {
            doc["answer_file_version"] = v;
            assert!(matches!(apply(&doc, AppConfig::default()), Err(AnswerFileError::Version { .. })));
        }
        assert_eq!(apply(&json!([1]), AppConfig::default()).unwrap_err(), AnswerFileError::NotAnObject);
    }

    #[test]
    fn answer_file_unknown_keys_are_refused_by_their_dotted_name() {
        for (path, key) in [(None, "web_host"), (Some("archive"), "path"), (Some("graph"), "write_access"), (Some("llm"), "auth")] {
            let mut doc = full();
            match path {
                None => doc[key] = json!("x"),
                Some(section) => doc[section][key] = json!("x"),
            }
            let want = path.map_or(key.to_string(), |s| format!("{s}.{key}"));
            assert_eq!(apply(&doc, AppConfig::default()).unwrap_err(), AnswerFileError::Unknown { key: want });
        }
    }

    #[test]
    fn answer_file_secrets_are_refused_and_sent_to_the_wizard() {
        for (section, key) in [
            (Some("graph"), "client_secret"),
            (Some("graph"), "secret_set"),
            (Some("llm"), "api_key"),
            (Some("tls"), "password"),
            (None, "session_token"),
            (None, "Admin_Password"),
        ] {
            let mut doc = full();
            match section {
                None => doc[key] = json!("value-test-only"),
                Some(s) => doc[s][key] = json!("value-test-only"),
            }
            let err = apply(&doc, AppConfig::default()).unwrap_err();
            assert!(matches!(err, AnswerFileError::Secret { .. }), "{key}: {err:?}");
            let text = err.to_string();
            assert!(text.contains("setup wizard") && !text.contains("value-test-only"), "{text}");
        }
    }

    #[test]
    fn answer_file_no_exported_key_looks_like_a_secret() {
        // Every key name in the export; a secret-like one would refuse every answer file.
        fn names(v: &Value, out: &mut Vec<String>) {
            if let Some(m) = v.as_object() {
                for (k, v) in m {
                    out.push(k.to_ascii_lowercase());
                    names(v, out);
                }
            }
        }
        let mut all = Vec::new();
        names(&full(), &mut all);
        for n in all {
            assert!(!SECRET_NAMES.iter().any(|s| n.contains(s)), "{n}");
        }
    }

    #[test]
    fn answer_file_wrong_types_are_refused_with_the_key() {
        let cases = [
            ("web_port", json!("8080")),
            ("web_port", json!(70000)),
            ("web_port", json!(0)),
            ("setup_mode", json!("expert")),
            ("setup_completed", json!("yes")),
            ("mcr_open_networks", json!("10.0.0.0/8")),
            ("archive", json!(true)),
        ];
        for (key, v) in cases {
            let mut doc = full();
            doc[key] = v;
            match apply(&doc, AppConfig::default()).unwrap_err() {
                AnswerFileError::WrongType { key: k, .. } => assert_eq!(k, key),
                e => panic!("{key}: {e:?}"),
            }
        }
        let mut doc = full();
        doc["llm"]["mode"] = json!("everything");
        assert!(matches!(apply(&doc, AppConfig::default()), Err(AnswerFileError::WrongType { key, .. }) if key == "llm.mode"));
        doc = full();
        doc["archive"]["retention_days"] = json!("30");
        assert!(matches!(apply(&doc, AppConfig::default()), Err(AnswerFileError::WrongType { key, .. }) if key == "archive.retention_days"));
    }
}
