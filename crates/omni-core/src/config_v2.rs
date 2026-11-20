//! Config v2 (P13.7): the flat keys of the first config.json move into their
//! sections, and every problem with a value names the key, the value and the
//! fix.
//!
//! * [`migrate_v1`] moves `ollama_endpoint` / `ollama_model` into `llm`,
//!   `email_poll_interval_secs` into `mail`, and drops the IMAP-era `imap_*`
//!   and `email_*` keys (IMAP is gone; the mailbox is `graph`). It works on the
//!   JSON, so every reader of config.json sees v2 whether or not the file has
//!   been rewritten yet.
//! * [`upgrade_file`] is what the daemon runs at start: when the file still has
//!   v1 keys it keeps a copy of it, secrets blanked, as `config.json.v1.bak`
//!   (`.v1.2.bak`… if that exists; a backup is never overwritten) and writes the
//!   v2 file.
//! * [`parse`] reads config.json; a value of the wrong kind is refused with its
//!   key, its value and the default to write instead.
//! * [`problems`] lists values the daemon reads differently from what they say
//!   (out of range), for the start-up log.
//!
//! `email_password` (the IMAP password) is left for
//! [`AppConfig::adopt_secrets`], which wipes it and says so.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::config::AppConfig;

/// One key moved or removed by [`migrate_v1`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// The v1 key.
    pub from: String,
    /// Where its value went; `None` when it was dropped.
    pub to: Option<String>,
    /// Why, for the log.
    pub note: String,
}

impl std::fmt::Display for Moved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.to {
            Some(to) => write!(f, "`{}` moved to `{to}`{}", self.from, if self.note.is_empty() { String::new() } else { format!(" ({})", self.note) }),
            None => write!(f, "`{}` removed: {}", self.from, self.note),
        }
    }
}

/// Whether `key` is a v1 top-level key that [`migrate_v1`] moves or drops.
fn is_v1_key(key: &str) -> bool {
    (key.starts_with("ollama_") || key.starts_with("imap_") || key.starts_with("email_")) && key != "email_password"
}

/// Move the v1 keys of `doc` into their v2 sections (see the module text).
/// A value already in the v2 place wins over the old one, which is dropped.
/// Returns what was done; empty for a v2 file.
pub fn migrate_v1(doc: &mut Value) -> Vec<Moved> {
    let Some(top) = doc.as_object_mut() else { return Vec::new() };
    let old: Vec<String> = top.keys().filter(|k| is_v1_key(k)).cloned().collect();
    let mut done = Vec::new();
    for key in old {
        let value = top.remove(&key).unwrap_or(Value::Null);
        let target = match key.as_str() {
            "ollama_endpoint" => Some(("llm", "endpoint")),
            "ollama_model" => Some(("llm", "model")),
            "email_poll_interval_secs" => Some(("mail", "poll_interval_secs")),
            _ => None,
        };
        let Some((section, field)) = target else {
            let note = if key.starts_with("ollama_") {
                "no longer used".to_string()
            } else {
                "IMAP is no longer supported; the mailbox is the `graph` section".to_string()
            };
            done.push(Moved { from: key, to: None, note });
            continue;
        };
        let to = format!("{section}.{field}");
        let entry = top.entry(section.to_string()).or_insert_with(|| Value::Object(Map::new()));
        match entry.as_object_mut() {
            Some(obj) if obj.contains_key(field) => {
                done.push(Moved { from: key, to: None, note: format!("`{to}` is already set and is kept") });
            }
            Some(obj) => {
                obj.insert(field.to_string(), value);
                done.push(Moved { from: key, to: Some(to), note: String::new() });
            }
            // `llm` or `mail` is not an object: leave it for `parse` to name.
            None => done.push(Moved { from: key, to: None, note: format!("`{section}` is not a section; fix it first") }),
        }
    }
    done
}

/// Keys whose value is a secret or may hold one: blanked in a backup.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["password", "secret", "api_key", "apikey", "token", "cookie"].iter().any(|s| k.contains(s))
}

/// `doc` with every secret-looking value replaced by `"<removed>"`, at any depth.
pub fn without_secrets(doc: &Value) -> Value {
    match doc {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = if is_secret_key(k) && !v.is_null() && v != &Value::String(String::new()) {
                        Value::String("<removed>".into())
                    } else {
                        without_secrets(v)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_secrets).collect()),
        other => other.clone(),
    }
}

/// What [`upgrade_file`] did.
#[derive(Debug)]
pub struct Upgrade {
    /// The copy of the v1 file, secrets blanked.
    pub backup: PathBuf,
    pub moved: Vec<Moved>,
}

/// `config.json.v1.bak`, or the first `config.json.v1.N.bak` not taken.
fn backup_path(config: &Path) -> PathBuf {
    let name = config.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "config.json".into());
    let first = config.with_file_name(format!("{name}.v1.bak"));
    if !first.exists() {
        return first;
    }
    (2..).map(|n| config.with_file_name(format!("{name}.v1.{n}.bak"))).find(|p| !p.exists()).expect("a free name")
}

/// Rewrite `config` as v2 when it still has v1 keys (P13.7); `None` when it
/// is v2 already or missing. The copy is written before the file is changed;
/// a failure to write either leaves config.json as it was.
pub fn upgrade_file(config: &Path) -> Result<Option<Upgrade>> {
    if !config.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(config).with_context(|| format!("Cannot read {}", config.display()))?;
    let Ok(mut doc) = serde_json::from_str::<Value>(&text) else {
        return Ok(None); // `parse` names what is wrong
    };
    let original = doc.clone();
    let moved = migrate_v1(&mut doc);
    if moved.is_empty() {
        return Ok(None);
    }
    let backup = backup_path(config);
    let copy = serde_json::to_string_pretty(&without_secrets(&original))?;
    std::fs::write(&backup, copy).with_context(|| format!("Cannot keep a copy of the old settings as {}; config.json is unchanged", backup.display()))?;
    let upgraded = from_value(doc, config)?;
    upgraded.save_to_file(config).with_context(|| format!("Cannot write the new settings to {}; the old file is unchanged", config.display()))?;
    Ok(Some(Upgrade { backup, moved }))
}

/// A value as the log may show it: a URL without credentials or query, a
/// string cut at 120 characters.
pub fn shown(value: &Value) -> String {
    match value {
        Value::String(s) => {
            let s = match s.split_once("://") {
                Some((scheme, rest)) => {
                    let rest = rest.split(['?', '#']).next().unwrap_or("");
                    let rest = match rest.split_once('/') {
                        Some((auth, path)) => format!("{}/{path}", auth.rsplit('@').next().unwrap_or(auth)),
                        None => rest.rsplit('@').next().unwrap_or(rest).to_string(),
                    };
                    format!("{scheme}://{rest}")
                }
                None => s.clone(),
            };
            let cut: String = s.chars().take(120).collect();
            format!("\"{cut}\"")
        }
        other => {
            let s = other.to_string();
            if s.chars().count() > 120 { format!("{}…", s.chars().take(120).collect::<String>()) } else { s }
        }
    }
}

/// The value at `path` (`a.b[2].c`) of `doc`.
fn at<'a>(doc: &'a Value, path: &serde_path_to_error::Path) -> Option<&'a Value> {
    use serde_path_to_error::Segment;
    let mut cur = doc;
    for seg in path.iter() {
        cur = match seg {
            Segment::Map { key } => cur.get(key.as_str())?,
            Segment::Seq { index } => cur.get(*index)?,
            Segment::Enum { .. } | Segment::Unknown => return Some(cur),
        };
    }
    Some(cur)
}

/// Read a migrated `doc` as the settings; a bad value names its key, its
/// value and the fix.
fn from_value(doc: Value, file: &Path) -> Result<AppConfig> {
    serde_path_to_error::deserialize::<_, AppConfig>(doc.clone()).map_err(|e| {
        let key = e.path().to_string();
        let value = at(&doc, e.path()).map(shown).unwrap_or_else(|| "(missing)".into());
        let default = serde_json::to_value(AppConfig::default()).ok().and_then(|d| at(&d, e.path()).cloned());
        let fix = match default {
            Some(Value::Null) | None => format!("correct it, or remove `{key}`"),
            Some(d) => format!("write a value like the default, {}, or remove `{key}` to use the default", shown(&d)),
        };
        anyhow::anyhow!(
            "{}: `{key}` = {value} cannot be read ({}). Fix: {fix}.",
            file.display(),
            e.inner()
        )
    })
}

/// Read config.json's text: not JSON, or a value of the wrong kind, is an
/// error that names where, what and the fix. v1 keys are read as v2.
pub fn parse(text: &str, file: &Path) -> Result<AppConfig> {
    let mut doc: Value = serde_json::from_str(text).map_err(|e| {
        anyhow::anyhow!(
            "{} is not valid JSON: {e}. Fix: correct line {} (a missing comma or quote is the usual cause), or restore {} from a copy (config.json.prev, config.json.v1.bak).",
            file.display(),
            e.line(),
            file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        )
    })?;
    migrate_v1(&mut doc);
    from_value(doc, file)
}

/// A value the daemon does not take as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub key: &'static str,
    pub value: String,
    pub fix: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` = {}: {}", self.key, self.value, self.fix)
    }
}

/// Values outside the range the daemon reads them in: it uses the nearest
/// allowed one, and the start-up log says so with the fix.
pub fn problems(c: &AppConfig) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut range = |key: &'static str, value: i64, lo: i64, hi: i64, what: &str| {
        if value < lo || value > hi {
            let used = value.clamp(lo, hi);
            out.push(Problem {
                key,
                value: value.to_string(),
                fix: format!("{what} is read as {lo}..={hi}, so {used} is used. Fix: set {key} to a value from {lo} to {hi}."),
            });
        }
    };
    range("max_concurrent_downloads", c.max_concurrent_downloads as i64, 1, 10, "the number of downloads at once");
    range("max_concurrent_transcodes", c.max_concurrent_transcodes as i64, 1, 8, "the number of encodes at once");
    range("mail.poll_interval_secs", c.mail.poll_interval_secs.min(i64::MAX as u64) as i64, 10, 3600, "the seconds between mailbox checks");
    range("retry.max_age_mins", c.retry.max_age_mins, 30, 360, "the age up to which a job is retried");
    range("pacing.per_host_concurrency", c.pacing.per_host_concurrency, 1, 4, "the yt-dlp runs per platform");
    range("pacing.min_gap_secs", c.pacing.min_gap_secs, 0, 120, "the seconds between two yt-dlp starts on one platform");
    out
}

/// Values the daemon cannot start with: the key, the value and the fix.
pub fn fatal(c: &AppConfig) -> Vec<Problem> {
    let mut out = Vec::new();
    if c.web_port == 0 {
        out.push(Problem { key: "web_port", value: "0".into(), fix: "the panels would listen on a random port. Fix: set web_port to the port the panels use, 8080 by default.".into() });
    }
    if c.watchfolder_path.trim().is_empty() {
        out.push(Problem {
            key: "watchfolder_path",
            value: "\"\"".into(),
            fix: "videos would be delivered into the installation folder. Fix: set watchfolder_path to the playout share (\\\\server\\share), or to \"watchfolder\" for the local folder.".into(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn v1_keys_move_into_their_sections_and_imap_keys_are_dropped() {
        let mut doc = json!({
            "ollama_endpoint": "http://127.0.0.1:1234/v1",
            "ollama_model": "model-test",
            "email_poll_interval_secs": 45,
            "imap_server": "mail.example.invalid",
            "imap_port": 993,
            "email_address": "ingest@station.example",
            "email_password": "kept-for-adopt-secrets",
            "llm": { "mode": "off" },
            "web_port": 8085,
        });
        let moved = migrate_v1(&mut doc);
        assert_eq!(doc["llm"], json!({ "mode": "off", "endpoint": "http://127.0.0.1:1234/v1", "model": "model-test" }));
        assert_eq!(doc["mail"], json!({ "poll_interval_secs": 45 }));
        for gone in ["ollama_endpoint", "ollama_model", "email_poll_interval_secs", "imap_server", "imap_port", "email_address"] {
            assert!(doc.get(gone).is_none(), "{gone} is still there");
        }
        assert_eq!(doc["email_password"], "kept-for-adopt-secrets", "left for adopt_secrets");
        assert_eq!(doc["web_port"], 8085);
        assert_eq!(moved.len(), 6);
        assert!(moved.iter().any(|m| m.from == "imap_server" && m.to.is_none() && m.note.contains("IMAP")));

        let mut again = doc.clone();
        assert!(migrate_v1(&mut again).is_empty(), "a v2 file is left alone");
        assert_eq!(again, doc);
    }

    #[test]
    fn a_value_already_in_the_v2_place_wins_over_the_old_one() {
        let mut doc = json!({ "ollama_endpoint": "http://old.example/v1", "llm": { "endpoint": "https://new.example/v1" } });
        let moved = migrate_v1(&mut doc);
        assert_eq!(doc["llm"]["endpoint"], "https://new.example/v1");
        assert!(moved[0].to.is_none() && moved[0].note.contains("already set"), "{moved:?}");
    }

    #[test]
    fn a_v1_file_is_upgraded_once_with_a_copy_that_holds_no_secret() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{ "web_port": 8091, "ollama_endpoint": "http://127.0.0.1:1234/v1", "email_poll_interval_secs": 30,
            "imap_server": "mail.example.invalid", "email_password": "imap-secret-test", "graph": { "mailbox": "ingest@station.example" } }"#).unwrap();

        let up = upgrade_file(&path).unwrap().expect("upgraded");
        assert_eq!(up.backup, dir.path().join("config.json.v1.bak"));
        let copy = std::fs::read_to_string(&up.backup).unwrap();
        assert!(copy.contains("imap_server") && copy.contains("8091"), "the copy is the old file");
        assert!(!copy.contains("imap-secret-test"), "no secret in the copy: {copy}");

        let text = std::fs::read_to_string(&path).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert!(doc.get("ollama_endpoint").is_none() && doc.get("imap_server").is_none() && doc.get("email_password").is_none(), "{text}");
        assert_eq!(doc["llm"]["endpoint"], "http://127.0.0.1:1234/v1");
        assert_eq!(doc["mail"]["poll_interval_secs"], 30);
        assert_eq!((doc["web_port"].as_u64(), doc["graph"]["mailbox"].as_str()), (Some(8091), Some("ingest@station.example")));

        assert!(upgrade_file(&path).unwrap().is_none(), "once");
        // A later v1 file (restored by hand) never overwrites the first copy.
        std::fs::write(&path, r#"{ "ollama_model": "model-test" }"#).unwrap();
        let second = upgrade_file(&path).unwrap().expect("upgraded");
        assert_eq!(second.backup, dir.path().join("config.json.v1.2.bak"));
        assert_eq!(std::fs::read_to_string(&up.backup).unwrap(), copy);
    }

    #[test]
    fn an_upgrade_of_a_file_with_a_bad_value_changes_nothing_and_names_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let v1 = r#"{ "ollama_model": "model-test", "web_port": "eighty" }"#;
        std::fs::write(&path, v1).unwrap();
        let err = upgrade_file(&path).unwrap_err().to_string();
        assert!(err.contains("`web_port` = \"eighty\""), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), v1, "config.json is as it was");
    }

    #[test]
    fn a_value_of_the_wrong_kind_names_the_key_the_value_and_the_default() {
        let file = Path::new("config.json");
        let err = parse(r#"{ "web_port": "eighty" }"#, file).unwrap_err().to_string();
        assert!(err.contains("`web_port` = \"eighty\""), "{err}");
        assert!(err.contains("default, 8080"), "{err}");

        let err = parse(r#"{ "security": { "trusted_proxies": ["10.0.0.5/32", 7] } }"#, file).unwrap_err().to_string();
        assert!(err.contains("`security.trusted_proxies[1]` = 7"), "{err}");

        // A v1 key with a bad value is named by its v2 place.
        let err = parse(r#"{ "email_poll_interval_secs": "often" }"#, file).unwrap_err().to_string();
        assert!(err.contains("`mail.poll_interval_secs` = \"often\""), "{err}");
        assert!(err.contains("default, 20"), "{err}");

        let err = parse("{ \"web_port\": 8080,, }", file).unwrap_err().to_string();
        assert!(err.contains("not valid JSON") && err.contains("line 1"), "{err}");
    }

    /// Every reader (the CLI, the installer) sees a v1 file as v2 before the
    /// daemon has rewritten it, and reading never writes.
    #[test]
    fn a_v1_file_reads_as_v2_without_being_rewritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let v1 = r#"{ "ollama_endpoint": "https://llm.example/v1", "ollama_model": "model-test", "email_poll_interval_secs": 40 }"#;
        std::fs::write(&path, v1).unwrap();
        let c = AppConfig::load_from_file(&path).unwrap();
        assert_eq!((c.llm.endpoint.as_str(), c.llm.model.as_str(), c.mail.poll_interval_secs), ("https://llm.example/v1", "model-test", 40));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), v1);
    }

    #[test]
    fn the_shipped_example_is_v2() {
        let text = include_str!("../../../config.example.json");
        let mut doc: Value = serde_json::from_str(text).unwrap();
        assert!(migrate_v1(&mut doc).is_empty(), "config.example.json still has v1 keys");
        let c = parse(text, Path::new("config.example.json")).unwrap();
        assert_eq!((c.llm.endpoint.as_str(), c.mail.poll_interval_secs), ("http://127.0.0.1:1234/v1", 30));
        assert!(problems(&c).is_empty() && fatal(&c).is_empty());
    }

    #[test]
    fn a_shown_value_has_no_credentials_or_query() {
        assert_eq!(shown(&json!("https://user:pw-test@llm.example/v1?key=abc")), "\"https://llm.example/v1\"");
        assert_eq!(shown(&json!("http://127.0.0.1:1234")), "\"http://127.0.0.1:1234\"");
        assert_eq!(shown(&json!(12)), "12");
    }

    #[test]
    fn out_of_range_values_name_the_key_the_value_and_what_is_used() {
        let mut c = AppConfig::default();
        assert!(problems(&c).is_empty(), "the defaults are in range");
        c.max_concurrent_downloads = 0;
        c.mail.poll_interval_secs = 2;
        let p = problems(&c);
        assert_eq!(p.iter().map(|p| p.key).collect::<Vec<_>>(), ["max_concurrent_downloads", "mail.poll_interval_secs"]);
        assert_eq!(p[1].value, "2");
        assert!(p[1].fix.contains("10 is used") && p[1].fix.contains("Fix: set mail.poll_interval_secs"), "{}", p[1]);

        assert!(fatal(&AppConfig::default()).is_empty());
        c.web_port = 0;
        c.watchfolder_path = "  ".into();
        assert_eq!(fatal(&c).iter().map(|p| p.key).collect::<Vec<_>>(), ["web_port", "watchfolder_path"]);
    }
}
