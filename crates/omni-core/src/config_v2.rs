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
/// By whole name or `_name` suffix (P13.7b): `max_tokens` and
/// `client_secret_expires` are settings, not secrets.
fn is_secret_key(key: &str) -> bool {
    const NAMES: [&str; 7] = ["password", "secret", "api_key", "apikey", "token", "cookie", "cookies"];
    let k = key.to_ascii_lowercase();
    NAMES.iter().any(|n| k == *n || k.ends_with(&format!("_{n}")))
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
    let mut moved = migrate_v1(&mut doc);
    if moved.is_empty() {
        return Ok(None);
    }
    // The rewrite drops the IMAP password; adopt_secrets will no longer see it to say so.
    if original.get("email_password").and_then(Value::as_str).is_some_and(|p| !p.is_empty()) {
        moved.push(Moved { from: "email_password".into(), to: None, note: "IMAP is no longer supported; the password is not kept anywhere".into() });
    }
    // Check first, copy only when the rewrite will happen (P13.7b): a bad
    // value or a file the service may not write leaves no copy behind.
    let upgraded = from_value(doc, config)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(config)
        .with_context(|| format!("{} cannot be written; it is read as v2 but left as it is", config.display()))?;
    let backup = backup_path(config);
    let copy = serde_json::to_string_pretty(&without_secrets(&original))?;
    std::fs::write(&backup, copy).with_context(|| format!("Cannot keep a copy of the old settings as {}; config.json is unchanged", backup.display()))?;
    if let Err(e) = upgraded.save_to_file(config) {
        let _ = std::fs::remove_file(&backup);
        return Err(e).with_context(|| format!("Cannot write the new settings to {}; the old file is unchanged", config.display()));
    }
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
        use serde_path_to_error::Segment;
        let key = e.path().to_string();
        // The value only through `shown` (P13.7b), and never under a secret's name.
        let secret = e.path().iter().any(|s| matches!(s, Segment::Map { key } if is_secret_key(key)));
        let value = if secret {
            "<removed>".to_string()
        } else {
            at(&doc, e.path()).map(shown).unwrap_or_else(|| "(missing)".into())
        };
        let default = serde_json::to_value(AppConfig::default()).ok().and_then(|d| at(&d, e.path()).cloned());
        let fix = match default {
            _ if secret => format!("remove `{key}`; secrets are kept in the encrypted store, not in config.json"),
            Some(Value::Null) | None => format!("correct it, or remove `{key}`"),
            Some(d) => format!("write a value like the default, {}, or remove `{key}` to use the default", shown(&d)),
        };
        anyhow::anyhow!("{}: `{key}` = {value} cannot be read ({}). Fix: {fix}.", file.display(), why(&e.inner().to_string()))
    })
}

/// What serde says is wrong, without the value it repeats ("invalid type:
/// string \"…\", expected u16" → "expected u16").
fn why(inner: &str) -> String {
    match inner.rfind(", expected ") {
        Some(i) => inner[i + 2..].chars().take(160).collect(),
        None if inner.starts_with("missing field") => inner.chars().take(160).collect(),
        None => "not a valid value here".to_string(),
    }
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

/// A key config.json has that no setting reads (P13.4b): ignored, usually a typo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey {
    /// `watchfoldr_path`, `graph.mailbx`.
    pub key: String,
    /// The known key it most resembles, if any is close.
    pub closest: Option<String>,
}

impl std::fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.closest {
            Some(c) => write!(f, "`{}` is not a setting and is ignored. Fix: did you mean `{c}`? Rename it, or remove it.", self.key),
            None => write!(f, "`{}` is not a setting and is ignored. Fix: remove it (config.example.json lists every key).", self.key),
        }
    }
}

/// Keys read but not written back, or kept for the warnings they give.
const KNOWN_EXTRA: &[&str] = &["email_password", "auth_mode", "system_prompt", "graph.client_secret_expires"];

/// Edit distance, for the closest known name.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

fn closest<'a>(key: &str, known: impl Iterator<Item = &'a String>) -> Option<String> {
    let limit = (key.chars().count() / 3).max(2);
    known.map(|k| (distance(key, k), k)).filter(|(d, _)| *d <= limit).min_by_key(|(d, _)| *d).map(|(_, k)| k.clone())
}

/// The keys of `doc` (top level, and one level into each section) that no
/// setting reads, with the closest known name. `_comment` keys are notes.
/// v1 keys count as known: they are read through [`migrate_v1`].
pub fn unknown_keys(doc: &Value) -> Vec<UnknownKey> {
    let mut doc = doc.clone();
    migrate_v1(&mut doc);
    let Some(top) = doc.as_object() else { return Vec::new() };
    let defaults = serde_json::to_value(AppConfig::default()).unwrap_or(Value::Null);
    let Some(known_top) = defaults.as_object() else { return Vec::new() };
    let extra = |key: &str| KNOWN_EXTRA.contains(&key) || key == "_comment";
    let mut out = Vec::new();
    for (key, value) in top {
        if extra(key) {
            continue;
        }
        let Some(default) = known_top.get(key) else {
            out.push(UnknownKey { key: key.clone(), closest: closest(key, known_top.keys()) });
            continue;
        };
        if let (Some(section), Some(known)) = (value.as_object(), default.as_object()) {
            for inner in section.keys() {
                let full = format!("{key}.{inner}");
                if !known.contains_key(inner) && !extra(inner) && !extra(&full) {
                    let near = closest(inner, known.keys()).map(|c| format!("{key}.{c}"));
                    out.push(UnknownKey { key: full, closest: near });
                }
            }
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

/// [`unknown_keys`] of the file at `path`; empty when it cannot be read.
pub fn unknown_keys_in_file(path: &Path) -> Vec<UnknownKey> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|d| unknown_keys(&d))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_unknown_key_is_named_with_the_closest_known_one() {
        let doc = json!({
            "watchfoldr_path": "\\\\nas-test\\ingest",
            "graph": { "mailbx": "ingest@station.example", "tenant_id": "t", "client_secret_expires": "2027-01-01", "_comment": "note" },
            "security": { "mcr_open_networks": [] },
            "completely_unrelated_setting": 1,
            "ollama_endpoint": "http://127.0.0.1:1234/v1",
            "email_password": "",
            "_comment": "note",
        });
        let u = unknown_keys(&doc);
        let named: Vec<(&str, Option<&str>)> = u.iter().map(|k| (k.key.as_str(), k.closest.as_deref())).collect();
        assert_eq!(
            named,
            [("completely_unrelated_setting", None), ("graph.mailbx", Some("graph.mailbox")), ("watchfoldr_path", Some("watchfolder_path"))],
            "{u:?}"
        );
        assert!(u[2].to_string().contains("did you mean `watchfolder_path`"), "{}", u[2]);
        assert!(unknown_keys(&serde_json::to_value(AppConfig::default()).unwrap()).is_empty(), "every written key is known");
        let example: Value = serde_json::from_str(include_str!("../../../config.example.json")).unwrap();
        assert!(unknown_keys(&example).is_empty(), "{:?}", unknown_keys(&example));
    }

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
            "imap_server": "mail.example.invalid", "email_password": "imap-secret-test",
            "graph": { "mailbox": "ingest@station.example", "client_secret": "nested-secret-test", "client_secret_expires": "2027-01-31" },
            "llm": { "api_key": "sk-nested-test", "max_tokens": 600 },
            "repair": { "llm_max_tokens": 900 },
            "extra_accounts": [ { "name": "a", "token": "array-secret-test" } ] }"#).unwrap();

        let up = upgrade_file(&path).unwrap().expect("upgraded");
        assert_eq!(up.backup, dir.path().join("config.json.v1.bak"));
        let copy = std::fs::read_to_string(&up.backup).unwrap();
        assert!(copy.contains("imap_server") && copy.contains("8091"), "the copy is the old file");
        for secret in ["imap-secret-test", "nested-secret-test", "sk-nested-test", "array-secret-test"] {
            assert!(!copy.contains(secret), "{secret} in the copy: {copy}");
        }
        // The IMAP password the rewrite drops is reported (P13.7b).
        assert!(up.moved.iter().any(|m| m.from == "email_password" && m.to.is_none()), "{:?}", up.moved);
        // Settings that only look like secrets are kept, so the copy can be restored as it is.
        let restored = parse(&copy, &up.backup).expect("the copy parses");
        assert_eq!((restored.llm.max_tokens, restored.repair.llm_max_tokens), (600, 900));
        assert_eq!(restored.graph.client_secret_expires.map(|d| d.to_string()).as_deref(), Some("2027-01-31"));

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
        // P13.7b: no copy is left behind, start after start.
        let _ = upgrade_file(&path);
        let left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["config.json"]);
    }

    /// P13.7b: the message carries the value only as `shown` gives it, and
    /// never a secret's.
    #[test]
    fn a_bad_value_message_holds_no_credential_query_or_secret() {
        let file = Path::new("config.json");
        let err = parse(r#"{ "web_port": "https://user:pw-test@host.example/x?key=query-secret-test" }"#, file).unwrap_err().to_string();
        assert!(err.contains("`web_port` = \"https://host.example/x\""), "{err}");
        for leak in ["pw-test", "query-secret-test", "user:"] {
            assert!(!err.contains(leak), "{leak} in: {err}");
        }
        assert!(err.contains("expected u16"), "{err}");

        let err = parse(r#"{ "email_password": 12345678 }"#, file).unwrap_err().to_string();
        assert!(err.contains("`email_password` = <removed>"), "{err}");
        assert!(!err.contains("12345678"), "{err}");

        let err = parse(r#"{ "llm": { "mode": "very-secret-mode" } }"#, file).unwrap_err().to_string();
        assert!(!err.contains("very-secret-mode") || err.contains("= \"very-secret-mode\""), "only in the shown value: {err}");
        assert!(err.contains("expected one of"), "{err}");
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
