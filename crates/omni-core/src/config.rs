use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::paths::AppPaths;

pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are a broadcast automation parsing engine for a Greek newsroom. Analyze the email and extract video asset links into a strict JSON array.

=== FEW-SHOT EXAMPLES ===

EXAMPLE 1 — Simple single-link numbered section:
Input:
"1.
Πάνω από 1 εκατ. φίλαθλοι των Knicks στην παρέλαση του NBA
https://www.lifo.gr/now/sport/eimaste-oloi-mia-oikogeneia-pano-apo-1-ekat-filathloi-ton-knicks-stin-parelasi-ton
ΓΙΑ ΠΛΑΝΑ: https://www.youtube.com/watch?v=mOMyiwJCX6I"
Output:
{
  "journalist_surname": "PAPADAKI",
  "jobs": [
    {"url": "https://www.youtube.com/watch?v=mOMyiwJCX6I", "index_str": "1", "keyword": "KNICKS", "confidence": 1.0}
  ]
}

EXAMPLE 2 — Multi-link section with sub-indexing:
Input:
"4.
Στο Ρεκόρ Γκίνες ο «Τζόναθαν» ως το γηραιότερο χερσαίο ζώο στον κόσμο
https://www.lifo.gr/now/world/rekor-gkines-i-194hroni-helona-tzonathan-einai-giraiotero-hersaio-zoo-ston-kosmo
ΓΙΑ ΠΛΑΝΑ: https://www.youtube.com/watch?v=aO8YWYaNoew&t=50s"
Output:
{
  "journalist_surname": "PAPADAKI",
  "jobs": [
    {"url": "https://www.youtube.com/watch?v=aO8YWYaNoew", "index_str": "4", "keyword": "TZONATHAN", "confidence": 1.0}
  ]
}

EXAMPLE 3 — Unnumbered topic with journalist override in body text and multi-link:
Input:
"ΑΔΕΡΦΟΣ 45χρονος +ΣΥΝΟΔΟΙΠΟΡΟΙ
ΠΗΓΗ:STAR CHANNEL
https://www.facebook.com/staralithies/videos/..."
Output:
{
  "journalist_surname": "NIKOLAOU",
  "jobs": [
    {"url": "https://www.facebook.com/staralithies/videos/...", "index_str": "1", "keyword": "STARCHANNEL", "confidence": 1.0}
  ]
}

=== RULES ===

1. JOURNALIST IDENTIFICATION
Scan the SUBJECT LINE FIRST for patterns like:
  - "ΕΠΙΚΑΙΡΟΤΗΤΑ ΣΟΦΙΑ ΔΗΜΗΤΡΙΟΥ" → surname = "DIMITRIOU"
  - "ΘΕΜΑΤΑ ΑΝΝΑΣ" → surname = "PAPADAKI" (ARIA = Anna Papadaki)
  - "ΕΠΙΚΑΙΡΟΤΗΤΑ ΑΝΤΩΝΗΣ ΓΕΩΡΓΙΟΥ" → surname = "GEORGIOU"
  - "ΓΙΑ ΜΟΝΤΑΖ ΣΤΟ ΠΡΕΜΙΕΡ" → no journalist in subject → scan body
If subject has no name, scan the BODY for patterns:
  - "ΝΑ ΠΕΡΑΣΤΟΥΝ ΣΤΟ ΟΝΟΜΑ ΤΗΣ ΝΙΚΟΛΑΟΥ" → surname = "NIKOLAOU"
  - "ΝΑ ΠΕΡΑΣΤΕΙ ΣΤΟ ΟΝΟΜΑ ΜΟΥ" → use CC/sender mailbox name
  - "ΓΙΑ" + person name (e.g., "Πλάνα για Πέτρο") → surname = "PETROS"
  - "Θέματα για Καστανάκη" → surname = "GEORGIOU"
  Convert Greek surnames to LATIN uppercase (e.g., "ΝΙΚΟΛΑΟΥ" → "NIKOLAOU", "ΠΑΠΑΔΑΚΗ" → "PAPADAKI", "ΓΕΩΡΓΙΟΥ" → "GEORGIOU").
  If no name found anywhere, set journalist_surname to "MCR".

2. STRICT LITERAL NUMBERING & SUB-INDEXING
- Copy the EXACT base number from the text. If text says "4." use "4", do NOT auto-increment.
- SINGLE-LINK sections: index_str is just the number (e.g., "3", "4", "10").
- MULTI-LINK sections: append uppercase A, B, C, D... (e.g., "1A","1B","1C","1D").

3. COMPLETE DOCUMENT PROCESSING
You must scan EVERY line. Do not stop early. Process every numbered section from 1 to N.

4. TIER 1 LINKS (confidence = 1.0)
Any URL containing: youtube.com, youtu.be, youtube.com/shorts, instagram.com/reel/, instagram.com/p/, facebook.com/reel/, facebook.com/share/r/, facebook.com/watch, tiktok.com, x.com/*/status/*/video/
When a section has BOTH a Tier 1 link AND a Tier 2 link, extract ONLY the Tier 1 link(s).

5. TIER 2 LINKS (confidence = 0.5)
News portals: neakriti.gr, lifo.gr, protothema.gr, newsit.gr, iefimerida.gr, athletiko.gr, carandmotor.gr, gazzetta.gr, star.gr, bbc.com, cnn.com.
Only extract Tier 2 if NO Tier 1 link exists in that section.

6. GIA PLANA / ΓΙΑ ΠΛΑΝΑ HANDLER
When a section contains the text "GIA PLANA:" or "ΓΙΑ ΠΛΑΝΑ:" followed by a link, treat that specific link as the video asset.

7. PHOTO-ONLY BYPASS
If the ENTIRE email contains ONLY image URLs (.jpg, .png, etc.) with no video links, return: {"journalist_surname":"MCR","jobs":[]}.

8. DETERMINISTIC KEYWORD GENERATION
For each job, generate "keyword" by extracting 1-2 distinctive title words, transliterating Greek to Latin uppercase, max 20 chars.

9. OUTPUT FORMAT
Respond with ONLY valid JSON:
{
  "journalist_surname": "SURNAME",
  "jobs": [
    {
      "url": "https://...",
      "index_str": "1A",
      "keyword": "KEYWORD",
      "confidence": 1.0
    }
  ]
}"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuthMode {
    #[serde(rename = "open_mcr")]
    OpenMcr,
    #[serde(rename = "strict")]
    Strict,
}

impl Default for AuthMode {
    fn default() -> Self {
        AuthMode::OpenMcr
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_db_path")]
    pub database_path: String,

    #[serde(default = "default_watchfolder")]
    pub watchfolder_path: String,

    #[serde(default = "default_temp_path")]
    pub temp_path: String,

    #[serde(default = "default_bin_dir")]
    pub bin_dir: String,

    #[serde(default = "default_web_port")]
    pub web_port: u16,

    #[serde(default = "default_web_host")]
    pub web_host: String,

    #[serde(default)]
    pub auth_mode: AuthMode,

    #[serde(default = "default_concurrent")]
    pub max_concurrent_downloads: usize,

    #[serde(default = "default_concurrent")]
    pub max_concurrent_transcodes: usize,

    // Email / Outlook configuration
    #[serde(default = "default_email_provider")]
    pub email_provider: String,

    #[serde(default = "default_imap_server")]
    pub imap_server: String,

    #[serde(default = "default_imap_port")]
    pub imap_port: u16,

    #[serde(default)]
    pub email_address: String,

    #[serde(default)]
    pub email_password: String,

    #[serde(default = "default_poll_interval")]
    pub email_poll_interval_secs: u64,

    // LLM configuration
    #[serde(default = "default_ollama_endpoint")]
    pub ollama_endpoint: String,

    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,

    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,

    // Update settings
    #[serde(default = "default_channel")]
    pub ytdl_channel: String,

    #[serde(default = "default_true")]
    pub ytdl_auto_update_nightly: bool,

    // Adblock settings
    #[serde(default = "default_true")]
    pub adblock_enabled: bool,

    #[serde(default = "default_true")]
    pub adblock_hagezi_enabled: bool,

    #[serde(default = "default_true")]
    pub adblock_greek_enabled: bool,

    #[serde(default = "default_true")]
    pub adblock_auto_update_nightly: bool,

    #[serde(default)]
    pub adblock_ubol_extension_enabled: bool,

    // Tool overrides
    pub ffmpeg_path: Option<String>,
    pub ffprobe_path: Option<String>,
    pub bmxtranswrap_path: Option<String>,
    pub ytdl_path: Option<String>,
}

fn default_db_path() -> String {
    "data/omni.db".to_string()
}
fn default_watchfolder() -> String {
    "C:/Users/Shado/Videos/test".to_string()
}
fn default_temp_path() -> String {
    "temp".to_string()
}
fn default_bin_dir() -> String {
    "bin".to_string()
}
fn default_web_port() -> u16 {
    8080
}
fn default_web_host() -> String {
    "0.0.0.0".to_string()
}
fn default_concurrent() -> usize {
    2
}
fn default_email_provider() -> String {
    "Outlook".to_string()
}
fn default_imap_server() -> String {
    "outlook.office365.com".to_string()
}
fn default_imap_port() -> u16 {
    993
}
fn default_poll_interval() -> u64 {
    20
}
fn default_ollama_endpoint() -> String {
    "http://localhost:11434/v1".to_string()
}
fn default_ollama_model() -> String {
    "google/gemma-4-e4b".to_string()
}
fn default_system_prompt() -> String {
    DEFAULT_SYSTEM_PROMPT.to_string()
}
fn default_channel() -> String {
    "stable".to_string()
}
fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            database_path: default_db_path(),
            watchfolder_path: default_watchfolder(),
            temp_path: default_temp_path(),
            bin_dir: default_bin_dir(),
            web_port: default_web_port(),
            web_host: default_web_host(),
            auth_mode: AuthMode::default(),
            max_concurrent_downloads: default_concurrent(),
            max_concurrent_transcodes: default_concurrent(),
            email_provider: default_email_provider(),
            imap_server: default_imap_server(),
            imap_port: default_imap_port(),
            email_address: String::new(),
            email_password: String::new(),
            email_poll_interval_secs: default_poll_interval(),
            ollama_endpoint: default_ollama_endpoint(),
            ollama_model: default_ollama_model(),
            system_prompt: default_system_prompt(),
            ytdl_channel: default_channel(),
            ytdl_auto_update_nightly: true,
            adblock_enabled: true,
            adblock_hagezi_enabled: true,
            adblock_greek_enabled: true,
            adblock_auto_update_nightly: true,
            adblock_ubol_extension_enabled: false,
            ffmpeg_path: None,
            ffprobe_path: None,
            bmxtranswrap_path: None,
            ytdl_path: None,
        }
    }
}

impl AppConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p = path.as_ref();
        if p.exists() {
            let content = std::fs::read_to_string(p)
                .with_context(|| format!("Failed reading config file at {:?}", p))?;
            let config: AppConfig = serde_json::from_str(&content)
                .with_context(|| format!("Failed parsing JSON from {:?}", p))?;
            Ok(config)
        } else {
            let config = AppConfig::default();
            config.save_to_file(p)?;
            Ok(config)
        }
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let p = path.as_ref();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed creating directory for config {:?}", parent))?;
        }
        let json_str = serde_json::to_string_pretty(self)
            .context("Failed serializing AppConfig to JSON")?;
        std::fs::write(p, json_str)
            .with_context(|| format!("Failed writing config file {:?}", p))?;
        Ok(())
    }

    /// Resolve a config path against the **install directory** (plan P0.1, W-10).
    ///
    /// Under the Windows service the process working directory is
    /// `C:\Windows\System32`, so the previous CWD-relative behaviour silently
    /// created a second, empty `data/omni.db` there while the operator stared at
    /// an empty queue. Callers that hold an [`AppPaths`] should prefer
    /// [`AppPaths::resolve`]; this method exists for the code paths that only
    /// have a config, and delegates to the same logic.
    pub fn resolve_path_in(&self, paths: &AppPaths, relative_or_absolute: &str) -> PathBuf {
        paths.resolve(relative_or_absolute)
    }

    /// Resolve against the discovered install root.
    ///
    /// Kept for call sites that have no `AppPaths` to hand. It discovers the
    /// root from the executable location, never from the CWD.
    pub fn resolve_path(&self, relative_or_absolute: &str) -> PathBuf {
        let p = Path::new(relative_or_absolute);
        if p.is_absolute() {
            return p.to_path_buf();
        }
        AppPaths::discover("config.json").resolve(relative_or_absolute)
    }
}
